//! The provider adapter for the configured model, when the caller supplies none.

use std::sync::Arc;

use tautly_provider::anthropic::AnthropicProvider;
use tautly_provider::compat::CompatProvider;
use tautly_provider::google::GoogleProvider;
use tautly_provider::openai::OpenAiProvider;
use tautly_provider::{Catalog, ModelId, Provider};
use url::Url;

use crate::auth::api_key;
use crate::config::{Config, ProviderEntry};

const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const OPENAI_URL: &str = "https://api.openai.com";
const GOOGLE_URL: &str = "https://generativelanguage.googleapis.com";

/// The adapter for `model`'s provider with its API key (environment, then keychain).
/// This may read the keychain, which can block.
pub(crate) fn provider_for(
    config: &Config,
    model: &ModelId,
    catalog: Arc<Catalog>,
) -> Result<Arc<dyn Provider>, String> {
    let head = model.as_str().split('/').next().unwrap_or("");
    let (name, entry) = match model.provider() {
        "compat" => {
            let compat = head.strip_prefix("compat:").unwrap_or("");
            let entry = config
                .providers
                .compat
                .iter()
                .find(|c| c.name == compat)
                .ok_or_else(|| {
                    format!("no [[providers.compat]] entry named `{compat}` for model {model}")
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
        other => return Err(format!("unknown provider `{other}` in model {model}")),
    };
    let lookup = api_key(&name, &entry).map_err(|e| e.to_string())?;
    let base = |default: &str| -> Result<Url, String> {
        match &entry.base_url {
            Some(u) => Ok(u.clone()),
            None => Url::parse(default).map_err(|e| e.to_string()),
        }
    };
    let need_key = |lookup_key: Option<String>| {
        lookup_key.ok_or_else(|| {
            let mut msg = format!("no API key for {name}: {}", key_hint(&name, &entry));
            if let Some(note) = &lookup.keychain_note {
                msg.push_str(&format!(" ({note})"));
            }
            msg
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
                .ok_or_else(|| format!("{name} has no base_url"))?;
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

    // Only paths that fail before any key lookup: tests never touch the real keychain.
    #[test]
    fn unknown_providers_and_missing_compat_entries_are_errors() {
        let cfg = Config::default();
        let catalog = Arc::new(Catalog::builtin());
        let err = provider_for(&cfg, &ModelId::from("acme/x"), Arc::clone(&catalog))
            .err()
            .unwrap();
        assert!(err.contains("unknown provider `acme`"), "{err}");
        let err = provider_for(&cfg, &ModelId::from("compat:local/llama"), catalog)
            .err()
            .unwrap();
        assert!(err.contains("`local`"), "{err}");
    }
}
