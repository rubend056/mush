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

use crate::config::DEFAULT_CONTEXT_TOKENS;

/// A model id and the context window its provider documents for it. Used when
/// the endpoint does not advertise one (`api.deepseek.com` answers with ids
/// only). Keep these honest: the value is shown wherever the model is chosen.
pub struct ModelSpec {
    pub id: &'static str,
    pub context_tokens: usize,
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
    /// Whether the provider requires an API key for normal use.
    pub needs_api_key: bool,
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
    /// The `reasoning_effort` a request sends when the human states none.
    /// `None` is "not stated": no such field is sent at all, because inventing
    /// one for an endpoint whose provider never documented it is how a request
    /// gets rejected.
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
        needs_api_key: true,
        models: &[
            ModelSpec {
                id: "deepseek-flash",
                context_tokens: 500_000,
            },
            ModelSpec {
                id: "deepseek-v4-pro",
                context_tokens: 500_000,
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
        needs_api_key: false,
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

    /// Whether the provider requires an API key for normal use.
    pub fn needs_api_key(&self) -> bool {
        self.spec().needs_api_key
    }
}

/// How a message names the providers a human can choose, e.g.
/// `deepseek or custom`. Built from the table so an error can never offer a
/// name `--provider` would reject.
pub fn names_hint() -> String {
    join_names(" or ", PROVIDERS.iter().map(|spec| spec.name))
}

/// The same list in the bracket form the help text uses for `/provider`, e.g.
/// `deepseek|custom`.
pub fn names_piped() -> String {
    join_names("|", PROVIDERS.iter().map(|spec| spec.name))
}

fn join_names(separator: &str, names: impl Iterator<Item = &'static str>) -> String {
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
/// e.g. `120000 for deepseek, 8192 for custom`. Spelled from the table like the
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
            "120000 for deepseek, 8192 for custom"
        );
        assert_eq!(thinking_default_hint(), "on for deepseek, off elsewhere");
        assert_eq!(
            effort_default_hint(),
            "`high` for deepseek, no field elsewhere"
        );
    }

    /// The window a provider falls back to when nobody stated one: for DeepSeek
    /// the number the human stated (120k), not a small guess that truncates
    /// ordinary work; for an endpoint mush knows nothing about, the small
    /// window a local server really has. Neither is a statement, so neither is
    /// ever stored as if a human had made it.
    #[test]
    fn the_fallback_windows_are_the_shipped_numbers() {
        assert_eq!(Provider::DeepSeek.spec().fallback_context_tokens, 120_000);
        assert_eq!(Provider::Custom.spec().fallback_context_tokens, 8_192);
    }

    #[test]
    fn a_documented_model_has_its_window() {
        assert_eq!(known_context("deepseek-flash"), Some(500_000));
        assert_eq!(known_context("vendor/deepseek-v4-pro"), Some(500_000));
        assert_eq!(known_context("qwen2.5-coder"), None);
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
