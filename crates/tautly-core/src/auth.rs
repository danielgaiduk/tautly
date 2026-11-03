//! API keys: an environment variable first, then the OS keychain. No other source.

use std::ffi::OsString;

use crate::config::ProviderEntry;

/// Keychain service name; the account is the provider (`anthropic`, `openai`, `google`,
/// `compat:<name>`).
pub const KEYCHAIN_SERVICE: &str = "tautly";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("environment variable {name} is not valid unicode")]
    EnvNotUnicode { name: String },
    #[error("keychain: {0}")]
    Keychain(String),
}

/// A place secrets live, so tests can inject a fake keychain.
pub trait SecretStore: Send + Sync {
    /// The secret stored for `service`/`account`; `Ok(None)` when there is none.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, AuthError>;
}

/// The operating system's keychain (macOS Keychain, Linux Secret Service).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl SecretStore for KeyringStore {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, AuthError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| AuthError::Keychain(e.to_string()))?;
        from_keyring(entry.get_password())
    }
}

/// "No such entry" is not an error; any other failure (locked keychain, no Secret Service
/// on a headless box) is.
fn from_keyring(result: Result<String, keyring::Error>) -> Result<Option<String>, AuthError> {
    match result {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(AuthError::Keychain(e.to_string())),
    }
}

/// The key for `provider`: the environment variable (`entry.api_key_env`, else the
/// provider's default: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`; none for
/// other providers), then the keychain (service `tautly`, account `provider`).
/// `Ok(None)` when neither has one. Surrounding whitespace is trimmed and a blank value
/// counts as unset. This reads the keychain, which can block: call it off the async
/// executor's threads.
pub fn api_key(provider: &str, entry: &ProviderEntry) -> Result<Option<String>, AuthError> {
    api_key_with(
        provider,
        entry,
        &|name| std::env::var_os(name),
        &KeyringStore,
    )
}

/// [`api_key`] with the environment lookup and the keychain injected.
pub fn api_key_with(
    provider: &str,
    entry: &ProviderEntry,
    env: &dyn Fn(&str) -> Option<OsString>,
    store: &dyn SecretStore,
) -> Result<Option<String>, AuthError> {
    let var = entry
        .api_key_env
        .as_deref()
        .or_else(|| default_env_var(provider));
    if let Some(name) = var {
        if let Some(raw) = env(name) {
            let text = raw.into_string().map_err(|_| AuthError::EnvNotUnicode {
                name: name.to_string(),
            })?;
            if let Some(key) = non_blank(&text) {
                return Ok(Some(key));
            }
        }
    }
    Ok(store
        .get(KEYCHAIN_SERVICE, provider)?
        .and_then(|secret| non_blank(&secret)))
}

fn default_env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        _ => None,
    }
}

fn non_blank(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_entry_is_none_other_failures_are_errors() {
        assert_eq!(from_keyring(Ok("k".into())).unwrap().as_deref(), Some("k"));
        assert_eq!(from_keyring(Err(keyring::Error::NoEntry)).unwrap(), None);
        let locked = keyring::Error::NoStorageAccess("locked".into());
        assert!(matches!(
            from_keyring(Err(locked)),
            Err(AuthError::Keychain(_))
        ));
    }
}
