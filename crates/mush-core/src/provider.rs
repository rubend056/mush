//! The provider table — the single place that knows a vendor by name.
//!
//! mush speaks OpenAI's chat API to anything that answers it, so a "provider"
//! is not a protocol: it is a bundle of defaults (endpoint, documented models,
//! context windows, thinking mode, reasoning effort) that a human may select
//! with `--provider`, `MUSH_PROVIDER`, or `/provider`. Everything mush *knows*
//! about a particular vendor is written down in [`PROVIDERS`] and nowhere else.
//!
//! Keeping it that way is the point of this module. A guess about one host —
//! the endpoint it lives at, the models it documents, the request fields its
//! models want — is a default, not a fact about the protocol, and a default
//! that leaks into `Config`, the request path, or the help text is a default
//! nobody can find again. Add a vendor here and every place that offers a
//! choice (`--provider`, the picker, the help, the home config's comments)
//! picks it up without being told; the test at the bottom of this module is
//! what keeps a vendor literal from creeping back out.
//!
//! The table is also **data**, not only a constant: the home config's
//! `providers` map adds rows by name ([`crate::userconfig::ProviderConfig`]),
//! and [`Catalog`] resolves both kinds into one owned table at startup. A row a
//! human wrote describes their own host — a LAN box serving a model mush has
//! never heard of — and is the reason a name, an endpoint and a set of model
//! facts no longer have to be baked into this binary to be usable. What stays
//! here is everything mush *knows*: a built-in row is still the only vendor
//! knowledge mush ships, and a config row can add a name but never take one over.

use crate::config::{ReasoningEffort, DEFAULT_CONTEXT_TOKENS};
use crate::userconfig::{config_path_label, ProviderConfig};

/// A model id and the facts its provider documents about it. Used when the
/// endpoint does not advertise them (`api.deepseek.com` answers with ids
/// only). Keep these honest: they are shown wherever the model is chosen.
pub struct ModelSpec {
    pub id: &'static str,
    pub context_tokens: usize,
    /// Whether this model's endpoint documents that it accepts image content
    /// parts. Off for everything the table does not state — including every
    /// model mush has never heard of — because a guess of "on" is not free:
    /// an endpoint that has never documented vision may reject the bytes, and a
    /// rejected request costs a whole turn (the actor hands the endpoint's
    /// complaint back and the run stops there). A model nobody has written a
    /// row for can still be asked, by text alone; if a human ever needs to
    /// override this the way they override a window, that switch belongs
    /// beside the other per-model facts, not as a default.
    pub vision: bool,
}

/// Everything vendor-specific about one selectable provider.
///
/// Every field is a *default*: each one is overruled by anything the human
/// states, and by what the endpoint itself advertises where that is knowable.
pub struct ProviderSpec {
    /// The variant this row describes. The table must hold exactly one row per
    /// [`Provider`] variant — `the_table_describes_every_variant` is what
    /// keeps that true.
    pub provider: Provider,
    /// The name a human types (`--provider`, `MUSH_PROVIDER`, `/provider`, and
    /// what the session and home config store).
    pub name: &'static str,
    /// Endpoint used when no URL is given.
    pub default_base_url: &'static str,
    /// The provider's documented models, used when the endpoint cannot list
    /// them (offline, missing key, or a server without `/v1/models`). Empty
    /// means "ask the endpoint and nothing else".
    pub models: &'static [ModelSpec],
    /// The window to assume when neither the endpoint nor [`ProviderSpec::models`]
    /// knows one. A default, never a statement: a `MUSH_CONTEXT`, a stored
    /// choice, or the endpoint's own advertised window all beat it.
    pub fallback_context_tokens: usize,
    /// Whether a request asks for the provider's thinking mode when the human
    /// states none.
    pub thinking_by_default: bool,
    /// The `reasoning_effort` a request sends when a human states no effort.
    /// `None` is "not stated": no such field is sent at all, because inventing
    /// one for an endpoint whose provider never documented it is how a request
    /// gets rejected — and with no spelling in the vocabulary meaning "send
    /// nothing", a row like this is the only way the field stays off.
    pub reasoning_effort_by_default: Option<&'static str>,
    /// Whether naming this provider also switches the endpoint to its own. A
    /// hosted API owns its endpoint, so selecting it must reach the right host;
    /// a provider that stands for "whatever the human pointed mush at" keeps
    /// the endpoint already set.
    pub switches_endpoint: bool,
    /// The endpoint shown in the status bar when it is the provider's own.
    /// `None` displays the configured URL instead — an endpoint the human
    /// typed is a fact worth seeing, theirs, not the vendor's.
    pub display_endpoint: Option<&'static str>,
}

/// The one place a vendor is named.
///
/// Order is the order the `/provider` picker lists them in.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        provider: Provider::DeepSeek,
        name: "deepseek",
        default_base_url: "https://api.deepseek.com",
        models: &[
            ModelSpec {
                id: "deepseek-flash",
                context_tokens: 500_000,
                // The one model mush talks to that documents image parts.
                vision: true,
            },
            ModelSpec {
                id: "deepseek-v4-pro",
                context_tokens: 500_000,
                vision: false,
            },
        ],
        // 120000 is the number the human stated for this provider, and it is
        // what the default is for: the modules below name a model's own
        // documented window when one is named, and this is the window a
        // session that has named none is budgeted against. Anything a human
        // states, and anything the endpoint advertises, still overrules it.
        fallback_context_tokens: 120_000,
        thinking_by_default: true,
        reasoning_effort_by_default: Some("high"),
        switches_endpoint: true,
        display_endpoint: Some("deepseek.com"),
    },
    ProviderSpec {
        provider: Provider::Custom,
        name: "custom",
        default_base_url: "http://rubendpc:8078",
        models: &[],
        fallback_context_tokens: DEFAULT_CONTEXT_TOKENS,
        thinking_by_default: false,
        reasoning_effort_by_default: None,
        // The endpoint is the human's, so naming this provider must not
        // quietly replace it with the one below.
        switches_endpoint: false,
        display_endpoint: None,
    },
];

/// The provider mush uses when nothing states one. [`Provider::Custom`], whose
/// endpoint is a LAN host: a request that was never pointed anywhere should not
/// travel to a vendor by accident.
pub const DEFAULT_PROVIDER: Provider = Provider::Custom;

/// Where mush talks to a model. Deliberately OpenAI-compatible so it works with
/// llama.cpp, Ollama, vLLM, LM Studio, and hosted APIs alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// A hosted vendor API, described by its row in [`PROVIDERS`].
    DeepSeek,
    /// Any OpenAI-compatible endpoint (local servers, proxies, other hosts).
    /// Carries no vendor knowledge of its own: what little it defaults to is
    /// its row in [`PROVIDERS`], and the endpoint is normally the human's.
    Custom,
}

impl Provider {
    /// Every provider, in the order the picker lists them.
    pub const ALL: [Provider; 2] = [Provider::DeepSeek, Provider::Custom];

    /// This provider's row in [`PROVIDERS`].
    pub fn spec(&self) -> &'static ProviderSpec {
        PROVIDERS
            .iter()
            .find(|spec| spec.provider == *self)
            .expect("every provider variant has a table row")
    }

    pub fn name(&self) -> &'static str {
        self.spec().name
    }

    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        // Deliberately no `openai`/`openai-compatible` aliases: they would land
        // on `Custom`, whose default endpoint is a LAN host, and the request
        // (and the API key) would go there.
        PROVIDERS
            .iter()
            .find(|spec| spec.name == name)
            .map(|spec| spec.provider)
    }

    /// Endpoint used when no URL is given.
    pub fn default_base_url(&self) -> &'static str {
        self.spec().default_base_url
    }

    /// This provider's row, owned: the shape the resolved [`Catalog`] holds and
    /// a [`crate::config::Config`] carries, so code that has a variant in hand
    /// can hand it to the same accessors a config-row name reaches.
    pub fn row(&self) -> ProviderRow {
        ProviderRow::of(*self)
    }
}

/// A model id and what the resolved table states about it — the one shape a
/// capability question is answered from, so a built-in row's documented facts
/// and a home config's annotations are read by the same accessor.
///
/// Both fields are `Option` because *unstated* is a third answer, not a `false`:
/// a model a row annotates `vision: false` is **known blind**, while a model
/// nothing names is **unknown** — an endpoint may well see, and only the human
/// can say. [`ProviderRow::vision`] keeps the two apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub id: String,
    pub context_tokens: Option<usize>,
    pub vision: Option<bool>,
}

/// One row of the resolved table: what a name selects, whether the row is
/// built in or came from the home config.
///
/// [`ProviderSpec`] holds `&'static` data because the built-in table is a
/// constant; a home config row is a `String` the file chose. This is the one
/// owned shape both are resolved into once at startup, so the rest of the code
/// asks a row by name and never has to know which kind it is holding — and
/// `Config` can carry it (it is `Clone`) where it used to carry a `Copy`
/// `Provider` variant.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRow {
    /// The name a human types, and what the session and home config store.
    pub name: String,
    /// The built-in row this came from. `None` is a row only the home config
    /// knows — the home box's own endpoint, say — which carries no vendor
    /// knowledge of its own.
    pub builtin: Option<Provider>,
    /// The endpoint selecting this row points at. Empty is a row that states
    /// none, and then selecting it keeps the endpoint already set (the same
    /// silence `custom`'s row is).
    pub base_url: String,
    /// The models this row names: a built-in row's documented ones, a config
    /// row's annotation set. Empty means "ask the endpoint and nothing else".
    pub models: Vec<ModelRow>,
    /// The window to assume when nothing documents one for the model in force.
    pub fallback_context_tokens: usize,
    /// Whether a request asks for the thinking mode when the human states none.
    pub thinking_by_default: bool,
    /// The `reasoning_effort` a request sends when the human states none.
    /// `None` is "not stated": no such field is sent at all, because inventing
    /// one for an endpoint whose provider never documented it is how a request
    /// gets rejected.
    pub reasoning_effort_by_default: Option<ReasoningEffort>,
    /// Whether naming this row also switches the endpoint to its own.
    pub switches_endpoint: bool,
    /// The endpoint shown in the status bar when it is the row's own. `None`
    /// displays the configured URL instead.
    pub display_endpoint: Option<String>,
    /// The temperature selecting this row applies. `None` — every built-in row
    /// — states none, so the temperature in force stands; only a home config row
    /// that says `temperature` replaces it, because that is a human stating a
    /// fact about a host they described, not mush guessing one.
    pub temperature: Option<f32>,
    /// The model selecting this row applies when nothing above states one.
    /// Empty for every built-in row: a vendor's documented models are already
    /// [`Self::models`], and picking one of those for the human is
    /// `default_models`' job.
    pub model: String,
}

/// A model id without its namespace (`vendor/model`): the spelling every
/// capability lookup compares by, so an endpoint's own prefixing cannot hide a
/// fact the table states.
fn without_namespace(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// What the built-in table states about a model id, if anything.
fn builtin_model(model: &str) -> Option<&'static ModelSpec> {
    let model = without_namespace(model);
    PROVIDERS
        .iter()
        .flat_map(|spec| spec.models)
        .find(|known| known.id == model)
}

impl ProviderRow {
    /// The built-in row of a variant, owned.
    pub fn of(provider: Provider) -> Self {
        let spec = provider.spec();
        Self {
            name: spec.name.to_string(),
            builtin: Some(provider),
            base_url: spec.default_base_url.to_string(),
            models: spec
                .models
                .iter()
                .map(|model| ModelRow {
                    id: model.id.to_string(),
                    context_tokens: Some(model.context_tokens),
                    vision: Some(model.vision),
                })
                .collect(),
            fallback_context_tokens: spec.fallback_context_tokens,
            thinking_by_default: spec.thinking_by_default,
            reasoning_effort_by_default: spec
                .reasoning_effort_by_default
                .and_then(|effort| ReasoningEffort::parse(effort).ok()),
            switches_endpoint: spec.switches_endpoint,
            display_endpoint: spec.display_endpoint.map(str::to_string),
            temperature: None,
            model: String::new(),
        }
    }

    /// A row the home config named, owned. The file's path is named with a bad
    /// effort for the reason every other home-config refusal names it:
    /// `MUSH_CONFIG` can point anywhere, so `home config` alone is not enough
    /// to find the line to fix.
    pub fn from_config(name: &str, row: &ProviderConfig) -> Result<Self, String> {
        let reasoning_effort_by_default = match row.reasoning_effort.as_deref() {
            None => None,
            Some(value) => Some(ReasoningEffort::parse(value).map_err(|error| {
                format!("home config: providers.{name}: {error} — {}", config_path_label())
            })?),
        };
        Ok(Self {
            name: name.to_string(),
            builtin: None,
            base_url: row.base_url.trim().trim_end_matches('/').to_string(),
            models: row
                .models
                .iter()
                .map(|(id, facts)| ModelRow {
                    id: id.clone(),
                    context_tokens: facts.context,
                    vision: facts.vision,
                })
                .collect(),
            fallback_context_tokens: row.context.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            // A row that states no thinking mode sends no `thinking` field, the
            // silence `custom`'s row keeps.
            thinking_by_default: row.thinking.unwrap_or(false),
            reasoning_effort_by_default,
            // A row that names an endpoint is a row that owns it: selecting it
            // must reach the host the human described. One that names none
            // stands for "wherever the human pointed mush" and keeps the
            // endpoint already set, as `custom` does.
            switches_endpoint: !row.base_url.trim().is_empty(),
            display_endpoint: None,
            temperature: row.temperature,
            model: row.model.clone(),
        })
    }

    /// The name a human types.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The model ids this row offers when the endpoint cannot list its own
    /// (offline, missing key, or a server without `/v1/models`): the row's own,
    /// in the order it wrote them.
    pub fn model_ids(&self) -> Vec<String> {
        self.models.iter().map(|model| model.id.clone()).collect()
    }

    /// The window a model id is documented to have: the built-in table first,
    /// then this row's own annotations.
    ///
    /// Built-ins first is the rule that keeps a home box's annotation from
    /// silently re-sizing a vendor's model, and it is the same order
    /// [`Catalog`] lists rows in. The id compares without its namespace
    /// (`vendor/model`).
    pub fn documented_context(&self, model: &str) -> Option<usize> {
        if let Some(known) = builtin_model(model) {
            return Some(known.context_tokens);
        }
        let model = without_namespace(model);
        self.models
            .iter()
            .find(|known| known.id == model)
            .and_then(|known| known.context_tokens)
    }

    /// Whether a model id accepts image content parts: `Some(true)` stated, or
    /// `Some(false)` for a row that is *known blind* — a built-in row that
    /// documents no vision, or an annotation saying so. `None` is a model the
    /// table states nothing about (see [`ModelRow`]).
    pub fn vision(&self, model: &str) -> Option<bool> {
        if let Some(known) = builtin_model(model) {
            // A built-in row documents the fact either way: a documented model
            // without vision is a model known not to take image parts.
            return Some(known.vision);
        }
        let model = without_namespace(model);
        self.models
            .iter()
            .find(|known| known.id == model)
            .and_then(|known| known.vision)
    }
}

/// Every name a human can select, resolved once at startup: the built-in rows
/// in the order [`PROVIDERS`] lists them, then the home config's rows in the
/// order its map keeps them (`providers` after the built-ins, so a file's row
/// can never take a built-in's place in the list).
///
/// This is the table `/provider`, the picker, `--provider` and the printed
/// config all ask by name; it rides in [`crate::config::Config`] because every
/// one of those surfaces reads a `Config`, and an actor's clone of it in
/// another thread must be able to answer with the same names the UI shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    rows: Vec<ProviderRow>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Catalog {
    /// The built-in rows alone: the table a `Config` built by code starts from,
    /// and the one a machine with no home config resolves to.
    pub fn builtin() -> Self {
        Self {
            rows: PROVIDERS
                .iter()
                .map(|spec| ProviderRow::of(spec.provider))
                .collect(),
        }
    }

    /// The built-ins plus the rows `home` names.
    ///
    /// A config row may not shadow a built-in name: two rows called `deepseek`
    /// would make `/provider deepseek` depend on which arm of the merge ran
    /// first, and the endpoint a request reaches would be decided by nothing a
    /// human could read. So it is a startup failure naming the file and the
    /// name — the same voice as the unknown-provider refusal (findings A17,
    /// C2).
    pub fn of_home(home: &crate::userconfig::UserConfig) -> Result<Self, String> {
        let mut catalog = Self::builtin();
        for (name, row) in &home.providers {
            let name = name.trim();
            if name.is_empty() {
                return Err(format!(
                    "home config: a provider row has no name — {}",
                    config_path_label()
                ));
            }
            if Provider::parse(name).is_some() {
                return Err(format!(
                    "home config: provider `{name}` shadows the built-in provider of that name \
                     — rename the row, and set its `base_url` from `--url` if that was the intent — {}",
                    config_path_label()
                ));
            }
            catalog.rows.push(ProviderRow::from_config(name, row)?);
        }
        Ok(catalog)
    }

    /// Every row, in the order the picker lists them.
    pub fn rows(&self) -> &[ProviderRow] {
        &self.rows
    }

    /// The row a name selects. Names compare the way `--provider` always has:
    /// trimmed and case-insensitive.
    pub fn get(&self, name: &str) -> Option<&ProviderRow> {
        let name = name.trim().to_ascii_lowercase();
        self.rows
            .iter()
            .find(|row| row.name.to_ascii_lowercase() == name)
    }

    /// The row a name selects, or the sentence an unknown name gets — built
    /// from this table's own names, so a refusal can never offer a row the
    /// table lost or hide one it gained.
    pub fn require(&self, name: &str) -> Result<&ProviderRow, String> {
        self.get(name).ok_or_else(|| {
            format!(
                "unknown provider `{}` (try {})",
                name.trim(),
                self.names_hint()
            )
        })
    }

    /// The row mush selects when nothing names one — `custom`, whose endpoint
    /// is a LAN host: a request never pointed anywhere must not travel to a
    /// vendor by accident.
    pub fn default_row(&self) -> &ProviderRow {
        self.get(DEFAULT_PROVIDER.name())
            .expect("the built-in rows are always in the table")
    }

    /// How a message names the providers a human can choose, e.g.
    /// `deepseek, custom or home`.
    pub fn names_hint(&self) -> String {
        join_names(" or ", self.rows.iter().map(|row| row.name.as_str()))
    }

    /// The same list in the bracket form the help text uses, e.g.
    /// `deepseek|custom|home`.
    pub fn names_piped(&self) -> String {
        join_names("|", self.rows.iter().map(|row| row.name.as_str()))
    }
}

/// How a message names the providers a human can choose, e.g.
/// `deepseek or custom`. Built from the table so an error can never offer a
/// name `--provider` would reject. The built-ins only: a sentence that never
/// saw a home config cannot promise a name that file adds
/// ([`Catalog::names_hint`] is the one that can).
pub fn names_hint() -> String {
    join_names(" or ", PROVIDERS.iter().map(|spec| spec.name))
}

/// The same list in the bracket form the help text uses for `/provider`, e.g.
/// `deepseek|custom`. Built-ins only, for the reason [`names_hint`] is.
pub fn names_piped() -> String {
    join_names("|", PROVIDERS.iter().map(|spec| spec.name))
}

fn join_names<'a>(separator: &str, names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{}{separator}{last}", rest.join(", ")),
    }
}

/// How the help text spells the thinking-mode default, e.g.
/// `on for deepseek, off elsewhere`. Spelled from the table so the sentence
/// cannot drift from what a request will actually send.
pub fn thinking_default_hint() -> String {
    let on: Vec<&str> = PROVIDERS
        .iter()
        .filter(|spec| spec.thinking_by_default)
        .map(|spec| spec.name)
        .collect();
    let off_somewhere = PROVIDERS.iter().any(|spec| !spec.thinking_by_default);
    if on.is_empty() {
        return "no `thinking` field anywhere".to_string();
    }
    let sentence = format!("on for {}", on.join(", "));
    if off_somewhere {
        format!("{sentence}, off elsewhere")
    } else {
        sentence
    }
}

/// How the home config's own header spells the built-in window per provider,
/// e.g. `120000 for deepseek, 32768 for custom`. Spelled from the table like the
/// other hints, so the number a human reads in a hand-editable file cannot
/// drift from the one a request would be sized against.
pub fn context_default_hint() -> String {
    PROVIDERS
        .iter()
        .map(|spec| format!("{} for {}", spec.fallback_context_tokens, spec.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How the help text spells the reasoning-effort default, e.g.
/// `high for deepseek, no field elsewhere`. Spelled from the table for the same
/// reason [`thinking_default_hint`] is.
pub fn effort_default_hint() -> String {
    let stated: Vec<String> = PROVIDERS
        .iter()
        .filter_map(|spec| {
            spec.reasoning_effort_by_default
                .map(|effort| format!("`{effort}` for {}", spec.name))
        })
        .collect();
    let silent_somewhere = PROVIDERS
        .iter()
        .any(|spec| spec.reasoning_effort_by_default.is_none());
    if stated.is_empty() {
        return "no `reasoning_effort` field anywhere".to_string();
    }
    let sentence = stated.join(", ");
    if silent_somewhere {
        format!("{sentence}, no field elsewhere")
    } else {
        sentence
    }
}

/// The window a model id is documented to have, if any provider in the table
/// documents one. The id is compared without its namespace (`vendor/model`).
pub fn known_context(model: &str) -> Option<usize> {
    let model = model.rsplit('/').next().unwrap_or(model);
    PROVIDERS
        .iter()
        .flat_map(|spec| spec.models)
        .find(|known| known.id == model)
        .map(|known| known.context_tokens)
}

/// Whether a model id is documented to accept image content parts
/// ([`ModelSpec::vision`]). `false` is the answer for a model no provider row
/// names: see the field — a capability mush cannot point at a document for is
/// not assumed, because being wrong about it costs a rejected request. The id
/// is compared without its namespace (`vendor/model`), like [`known_context`].
pub fn vision_capable(model: &str) -> bool {
    let model = model.rsplit('/').next().unwrap_or(model);
    PROVIDERS
        .iter()
        .flat_map(|spec| spec.models)
        .find(|known| known.id == model)
        .is_some_and(|known| known.vision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The table is the source of truth, so it must describe every variant
    /// exactly once and in the order `ALL` lists them.
    #[test]
    fn the_table_describes_every_variant() {
        let described: Vec<Provider> = PROVIDERS.iter().map(|spec| spec.provider).collect();
        assert_eq!(described, Provider::ALL, "one row per variant, in order");
        for spec in PROVIDERS {
            assert!(
                !spec.name.is_empty() && spec.name == spec.name.to_ascii_lowercase(),
                "`{}` is not a name a human can type",
                spec.name
            );
            assert!(
                spec.default_base_url.starts_with("http"),
                "`{}` has no usable default endpoint",
                spec.name
            );
        }
    }

    /// A name resolves to its own row, and a name no row carries resolves to
    /// nothing — the two directions `--provider` and the picker rely on.
    #[test]
    fn names_round_trip_through_the_table() {
        for spec in PROVIDERS {
            assert_eq!(Provider::parse(spec.name), Some(spec.provider));
            assert_eq!(spec.provider.name(), spec.name);
        }
        assert_eq!(Provider::parse(" deepseek "), Some(Provider::DeepSeek));
        assert_eq!(Provider::parse("openai"), None);
        assert_eq!(Provider::parse("claude"), None);
    }

    /// The hints are spelled from the table, so they name exactly the providers
    /// a human can pick.
    #[test]
    fn the_hints_are_spelled_from_the_table() {
        assert_eq!(names_hint(), "deepseek or custom");
        assert_eq!(names_piped(), "deepseek|custom");
        assert_eq!(
            context_default_hint(),
            "120000 for deepseek, 32768 for custom"
        );
        assert_eq!(thinking_default_hint(), "on for deepseek, off elsewhere");
        assert_eq!(
            effort_default_hint(),
            "`high` for deepseek, no field elsewhere"
        );
    }

    /// The window a provider falls back to when nobody stated one: for DeepSeek
    /// the number the human stated (120k), not a small guess that truncates
    /// ordinary work; for an endpoint mush knows nothing about, the window mush
    /// assumes by default ([`DEFAULT_CONTEXT_TOKENS`]). Neither is a statement,
    /// so neither is ever stored as if a human had made it.
    #[test]
    fn the_fallback_windows_are_the_shipped_numbers() {
        assert_eq!(Provider::DeepSeek.spec().fallback_context_tokens, 120_000);
        assert_eq!(Provider::Custom.spec().fallback_context_tokens, 32_768);
    }

    #[test]
    fn a_documented_model_has_its_window() {
        assert_eq!(known_context("deepseek-flash"), Some(500_000));
        assert_eq!(known_context("vendor/deepseek-v4-pro"), Some(500_000));
        assert_eq!(known_context("qwen2.5-coder"), None);
    }

    /// Vision is a per-model fact the table states, and a row that carries it is
    /// the only way to get a `true`. Everything else — a model the table names
    /// without vision, and every model it has never heard of — is `false`,
    /// because bytes sent to an endpoint that cannot take them cost a whole
    /// turn; being wrong in this direction costs an image, which a human can
    /// ask for by other means.
    #[test]
    fn vision_is_a_row_fact_and_unknown_models_are_not_assumed_to_see() {
        assert!(vision_capable("deepseek-flash"), "the table says so");
        assert!(
            vision_capable("vendor/deepseek-flash"),
            "the namespace is not part of the id"
        );
        assert!(
            !vision_capable("deepseek-v4-pro"),
            "named by the table, and not documented for images"
        );
        assert!(
            !vision_capable("qwen2.5-coder"),
            "a model no row names does not default on"
        );
        assert!(
            !vision_capable(""),
            "and neither does a model id that is nothing"
        );

        let seeing: Vec<&str> = PROVIDERS
            .iter()
            .flat_map(|spec| spec.models)
            .filter(|model| model.vision)
            .map(|model| model.id)
            .collect();
        assert_eq!(
            seeing,
            ["deepseek-flash"],
            "and exactly one row documents it today"
        );
    }

    /// A row the home config names is a provider like any other: the table is
    /// the built-ins first, then the file's rows in name order, and a name
    /// resolves however a human spelled it.
    #[test]
    fn a_config_row_adds_a_name_after_the_built_ins() {
        let home = crate::userconfig::UserConfig {
            providers: std::collections::BTreeMap::from([
                ("work".to_string(), crate::userconfig::ProviderConfig::default()),
                (
                    "home".to_string(),
                    crate::userconfig::ProviderConfig {
                        base_url: "http://box:9".to_string(),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let catalog = Catalog::of_home(&home).expect("no row shadows a built-in");
        let names: Vec<&str> = catalog.rows().iter().map(ProviderRow::name).collect();
        assert_eq!(names, ["deepseek", "custom", "home", "work"]);
        assert_eq!(catalog.names_hint(), "deepseek, custom, home or work");
        assert_eq!(
            catalog.get(" HOME ").map(ProviderRow::name),
            Some("home"),
            "a name resolves however it is spelled"
        );
        assert_eq!(catalog.get("deepseek").and_then(|row| row.builtin), Some(Provider::DeepSeek));
        assert_eq!(
            catalog.default_row().name(),
            DEFAULT_PROVIDER.name(),
            "a row from the file never becomes the default"
        );
    }

    /// A config row may not take a built-in name over: two rows called
    /// `deepseek` would make the endpoint a request reaches depend on which arm
    /// of the merge ran first, and no surface could say which one won. It is a
    /// startup failure naming the file and the name, in the same voice as the
    /// unknown-provider refusal.
    #[test]
    fn a_config_row_that_shadows_a_built_in_is_refused_naming_the_file() {
        let home = crate::userconfig::UserConfig {
            providers: std::collections::BTreeMap::from([(
                "deepseek".to_string(),
                crate::userconfig::ProviderConfig::default(),
            )]),
            ..Default::default()
        };
        let error = Catalog::of_home(&home).unwrap_err();
        assert!(error.contains("deepseek"), "{error}");
        assert!(error.contains("shadows"), "{error}");
        assert!(
            error.contains(&crate::userconfig::config_path_label()),
            "the file is named: {error}"
        );
    }

    /// A row's facts are what a capability question reads, and an annotation
    /// that states nothing is not an annotation saying no.
    #[test]
    fn a_config_rows_annotations_answer_for_models_the_table_never_named() {
        let home = crate::userconfig::UserConfig {
            providers: std::collections::BTreeMap::from([(
                "home".to_string(),
                crate::userconfig::ProviderConfig {
                    base_url: "http://box:9".to_string(),
                    models: std::collections::BTreeMap::from([
                        (
                            "lfm2-vl".to_string(),
                            crate::userconfig::ModelConfig {
                                vision: Some(true),
                                context: Some(32_768),
                            },
                        ),
                        (
                            "blind-box".to_string(),
                            crate::userconfig::ModelConfig {
                                vision: Some(false),
                                context: None,
                            },
                        ),
                        (
                            "silent".to_string(),
                            crate::userconfig::ModelConfig::default(),
                        ),
                    ]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let catalog = Catalog::of_home(&home).unwrap();
        let row = catalog.get("home").unwrap();
        assert_eq!(row.vision("lfm2-vl"), Some(true));
        assert_eq!(row.vision("vendor/lfm2-vl"), Some(true), "namespaces are not part of the id");
        assert_eq!(row.vision("blind-box"), Some(false));
        assert_eq!(row.vision("silent"), None, "an annotation stating no vision fact states nothing");
        assert_eq!(row.vision("qwen2.5-coder"), None, "and a model nothing names is unknown");
        assert_eq!(row.documented_context("lfm2-vl"), Some(32_768));
        assert_eq!(
            row.documented_context("deepseek-flash"),
            Some(500_000),
            "the built-in table is read first, so a row cannot re-size a vendor's model"
        );
        assert_eq!(row.model_ids(), ["blind-box", "lfm2-vl", "silent"]);
    }

    /// The guard this module exists for: production code outside this file
    /// must not name a vendor, its endpoint, or its models. Every needle is
    /// taken from the table itself, so adding a provider extends the check.
    ///
    /// Endpoints, hosts and model ids are checked bare *and* quoted — they are
    /// never ordinary prose, so an error message or a help line that spells one
    /// out is a default that has escaped. A provider's *name* is checked quoted
    /// only: `custom` is also an English word, and a doc comment that cites a
    /// vendor by name is explaining, not configuring.
    #[test]
    fn vendor_knowledge_stays_in_this_module() {
        let needles: Vec<String> = PROVIDERS
            .iter()
            .flat_map(|spec| {
                let mut out = vec![
                    format!("\"{}\"", spec.name),
                    format!("\"{}\"", spec.default_base_url),
                    spec.default_base_url.to_string(),
                ];
                if let Some(host) = spec.default_base_url.split("//").nth(1) {
                    let host = host.trim_end_matches('/');
                    out.push(host.to_string());
                    out.push(format!("\"{host}\""));
                }
                if let Some(endpoint) = spec.display_endpoint {
                    out.push(endpoint.to_string());
                    out.push(format!("\"{endpoint}\""));
                }
                for model in spec.models {
                    out.push(model.id.to_string());
                    out.push(format!("\"{}\"", model.id));
                }
                out
            })
            .collect();

        let mut checked = 0;
        for file in rust_sources(&mush_core_src()) {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            if name == "provider.rs" {
                continue;
            }
            check(&file, &name, &needles);
            checked += 1;
        }
        for file in rust_sources(&mush_src()) {
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            check(&file, &name, &needles);
            checked += 1;
        }
        assert!(checked > 10, "the walk found only {checked} files");
    }

    /// Only the production half of a file is checked: a test may name whatever
    /// it likes, and the fixtures they build are not what ships.
    fn check(file: &Path, name: &str, needles: &[String]) {
        let source = fs::read_to_string(file).expect("readable source");
        let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
        for needle in needles {
            assert!(
                !production.contains(needle.as_str()),
                "{name} names a vendor ({needle}) outside `provider.rs`; \
                 every vendor default belongs in the table there"
            );
        }
    }

    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
        out
    }

    fn mush_core_src() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    fn mush_src() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the mush crate sits beside mush-core")
            .join("mush/src")
    }
}
