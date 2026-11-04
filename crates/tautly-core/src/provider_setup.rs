//! The provider adapter for a model, when the caller supplies none: at session start for
//! the configured model, and on every `SetModel` (through a [`ProviderFactory`]).

use std::sync::Arc;

use tautly_provider::anthropic::AnthropicProvider;
use tautly_provider::compat::CompatProvider;
use tautly_provider::google::GoogleProvider;
use tautly_provider::openai::OpenAiProvider;
use tautly_provider::{Catalog, ModelId, Provider};
use url::Url;

use std::ffi::OsString;

use crate::auth::{KeyringStore, SecretStore, api_key_with};
use crate::config::{Config, ProviderEntry};

const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const OPENAI_URL: &str = "https://api.openai.com";
const GOOGLE_URL: &str = "https://generativelanguage.googleapis.com";

/// Builds the provider for a model (`SetModel`). It may read the keychain, which can block,
/// so the engine calls it on the blocking pool. The default is [`provider_for`] with the
/// session's configuration; tests inject their own so they never touch the keychain.
pub type ProviderFactory =
    Arc<dyn Fn(&ModelId) -> Result<Arc<dyn Provider>, ProviderSetupError> + Send + Sync>;

/// Why no provider could be set up for a model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderSetupError {
    #[error("unknown provider `{provider}` in model {model}")]
    UnknownProvider { provider: String, model: ModelId },
    #[error("`{model}` is not a model id: it names no model after the provider")]
    InvalidModelId { model: ModelId },
    #[error("no [[providers.compat]] entry named `{name}` for model {model}")]
    NoCompatEntry { name: String, model: ModelId },
    /// `provider` is `anthropic`, `openai`, `google` or `compat:<name>`; `how` says where
    /// the key is looked for, `note` why the keychain was not asked (if it was not).
    #[error(
        "no API key for {provider}: {how}{}",
        note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default()
    )]
    MissingKey {
        provider: String,
        how: String,
        note: Option<String>,
    },
    /// The keychain failed, or the provider's base URL is unusable.
    #[error("{0}")]
    Other(String),
}

impl ProviderSetupError {
    /// What the user can do about it, for `Error.hint`.
    pub fn hint(&self) -> String {
        match self {
            ProviderSetupError::UnknownProvider { .. }
            | ProviderSetupError::InvalidModelId { .. } => {
                "use a model id of the form <provider>/<model> with provider anthropic, openai, \
                 google or compat:<name>"
                    .to_string()
            }
            ProviderSetupError::NoCompatEntry { name, .. } => {
                format!("add a [[providers.compat]] entry named `{name}` to the configuration")
            }
            ProviderSetupError::MissingKey { provider, .. } => format!(
                "store a key with `tautly auth add {provider}`, or set its environment variable"
            ),
            ProviderSetupError::Other(_) => {
                "check the provider's configuration and the keychain".to_string()
            }
        }
    }
}

/// The provider a model belongs to, as `ModelChanged` names it: `anthropic`, `openai`,
/// `google`, `compat:<name>`, or whatever prefix an id has (`mock`).
pub fn provider_label(model: &ModelId) -> String {
    match model.provider() {
        "compat" => model
            .as_str()
            .split('/')
            .next()
            .unwrap_or("compat")
            .to_string(),
        other => other.to_string(),
    }
}

/// Whether `model` names a provider Tautly has an adapter for: `<provider>/<model>` with
/// provider `anthropic`, `openai`, `google` or `compat:<name>`, and a model after the `/`.
/// No configuration is read: a `compat:<name>` still needs its `[[providers.compat]]`
/// entry when the session starts.
pub fn known_provider(model: &ModelId) -> Result<(), ProviderSetupError> {
    let head = model.as_str().split('/').next().unwrap_or("");
    let known = match model.provider() {
        "anthropic" | "openai" | "google" => !head.contains(':'),
        "compat" => head
            .strip_prefix("compat:")
            .is_some_and(|name| !name.is_empty()),
        _ => false,
    };
    if !known {
        return Err(ProviderSetupError::UnknownProvider {
            provider: head.to_string(),
            model: model.clone(),
        });
    }
    if !model.as_str().contains('/') || model.model().is_empty() {
        return Err(ProviderSetupError::InvalidModelId {
            model: model.clone(),
        });
    }
    Ok(())
}

/// The adapter for `model`'s provider with its API key (environment, then keychain).
/// This may read the keychain, which can block.
pub(crate) fn provider_for(
    config: &Config,
    model: &ModelId,
    catalog: Arc<Catalog>,
) -> Result<Arc<dyn Provider>, ProviderSetupError> {
    provider_for_with(
        config,
        model,
        catalog,
        &|name| std::env::var_os(name),
        &KeyringStore,
    )
}

/// [`provider_for`] with the environment and the keychain injected (a client that checks
/// the key before it starts a session; tests). Compat models use
/// [`Config::effective_compat`], so the built-in `openrouter` and `ollama` entries work
/// without configuration. A native provider (`anthropic`, `openai`, `google`) without a
/// key is [`ProviderSetupError::MissingKey`]; a compat endpoint may have none.
pub fn provider_for_with(
    config: &Config,
    model: &ModelId,
    catalog: Arc<Catalog>,
    env: &dyn Fn(&str) -> Option<OsString>,
    store: &dyn SecretStore,
) -> Result<Arc<dyn Provider>, ProviderSetupError> {
    known_provider(model)?;
    let head = model.as_str().split('/').next().unwrap_or("");
    let (name, entry) = match model.provider() {
        "compat" => {
            let compat = head.strip_prefix("compat:").unwrap_or("");
            let entry = config
                .effective_compat()
                .into_iter()
                .find(|c| c.name == compat)
                .ok_or_else(|| ProviderSetupError::NoCompatEntry {
                    name: compat.to_string(),
                    model: model.clone(),
                })?;
            (format!("compat:{compat}"), entry.provider_entry())
        }
        p @ ("anthropic" | "openai" | "google") => {
            let entry = match p {
                "anthropic" => config.providers.anthropic.clone(),
                "openai" => config.providers.openai.clone(),
                _ => config.providers.google.clone(),
            };
            (p.to_string(), entry.unwrap_or_default())
        }
        other => {
            return Err(ProviderSetupError::UnknownProvider {
                provider: other.to_string(),
                model: model.clone(),
            });
        }
    };
    let lookup = api_key_with(&name, &entry, env, store)
        .map_err(|e| ProviderSetupError::Other(e.to_string()))?;
    let base = |default: &str| -> Result<Url, ProviderSetupError> {
        match &entry.base_url {
            Some(u) => Ok(u.clone()),
            None => Url::parse(default).map_err(|e| ProviderSetupError::Other(e.to_string())),
        }
    };
    let need_key = |lookup_key: Option<String>| {
        lookup_key.ok_or_else(|| ProviderSetupError::MissingKey {
            provider: name.clone(),
            how: key_hint(&name, &entry),
            note: lookup.keychain_note.clone(),
        })
    };
    Ok(match name.as_str() {
        "anthropic" => Arc::new(AnthropicProvider::new(
            need_key(lookup.key.clone())?,
            base(ANTHROPIC_URL)?,
            catalog,
        )),
        "openai" => Arc::new(OpenAiProvider::new(
            need_key(lookup.key.clone())?,
            base(OPENAI_URL)?,
            catalog,
        )),
        "google" => Arc::new(GoogleProvider::new(
            need_key(lookup.key.clone())?,
            base(GOOGLE_URL)?,
            catalog,
        )),
        _ => {
            let url = entry
                .base_url
                .clone()
                .ok_or_else(|| ProviderSetupError::Other(format!("{name} has no base_url")))?;
            Arc::new(CompatProvider::new(lookup.key.clone(), url, catalog))
        }
    })
}

fn key_hint(provider: &str, entry: &ProviderEntry) -> String {
    let var = entry.api_key_env.as_deref().or(match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        _ => None,
    });
    match var {
        Some(v) => format!("set {v} or store the key in the keychain (service `tautly`)"),
        None => {
            "set api_key_env for it or store the key in the keychain (service `tautly`)".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_are_the_adapters() {
        for ok in [
            "anthropic/claude-sonnet-5-5",
            "openai/gpt-5",
            "google/gemini-3-pro",
            "compat:local/qwen3",
        ] {
            assert_eq!(known_provider(&ModelId::from(ok)), Ok(()), "{ok}");
        }
        for bad in [
            "gpt-5",
            "foo/bar",
            "compat:/x",
            "compat/x",
            "anthropic/",
            "/x",
            "",
        ] {
            let err = known_provider(&ModelId::from(bad)).unwrap_err();
            assert!(
                matches!(
                    err,
                    ProviderSetupError::UnknownProvider { .. }
                        | ProviderSetupError::InvalidModelId { .. }
                ),
                "{bad}: {err}"
            );
            assert!(err.hint().contains("<provider>/<model>"), "{bad}");
        }
        assert!(matches!(
            known_provider(&ModelId::from("anthropic/")),
            Err(ProviderSetupError::InvalidModelId { .. })
        ));
    }

    // Only paths that fail before any key lookup: tests never touch the real keychain.
    #[test]
    fn unknown_providers_and_missing_compat_entries_are_errors() {
        let cfg = Config::default();
        let catalog = Arc::new(Catalog::builtin());
        let err = provider_for(&cfg, &ModelId::from("acme/x"), Arc::clone(&catalog))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("unknown provider `acme`"), "{err}");
        let err = provider_for(&cfg, &ModelId::from("compat:local/llama"), catalog)
            .err()
            .unwrap();
        assert!(err.to_string().contains("`local`"), "{err}");
        assert!(err.hint().contains("`local`"));
    }

    #[test]
    fn missing_keys_keep_their_message_and_point_at_auth_add() {
        let err = ProviderSetupError::MissingKey {
            provider: "openai".into(),
            how: key_hint("openai", &ProviderEntry::default()),
            note: Some("keychain not used: no bus".into()),
        };
        assert_eq!(
            err.to_string(),
            "no API key for openai: set OPENAI_API_KEY or store the key in the keychain \
             (service `tautly`) (keychain not used: no bus)"
        );
        assert!(
            err.hint().contains("tautly auth add openai"),
            "{}",
            err.hint()
        );
    }

    #[test]
    fn provider_labels_name_the_compat_endpoint() {
        assert_eq!(provider_label(&ModelId::from("anthropic/x")), "anthropic");
        assert_eq!(
            provider_label(&ModelId::from("compat:local/llama")),
            "compat:local"
        );
        assert_eq!(
            provider_label(&ModelId::from("google/gemini-3-pro")),
            "google"
        );
    }
}
