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
    /// The keychain answered with a failure that is not about the entry.
    #[error("keychain: {0}")]
    Keychain(String),
    /// There is no keychain to ask: no Secret Service daemon or session bus, or the
    /// platform store refused access. [`api_key`] treats this as "no key" and says why.
    #[error("keychain unavailable: {0}")]
    KeychainUnavailable(String),
}

/// A place secrets live, so tests can inject a fake keychain.
pub trait SecretStore: Send + Sync {
    /// The secret stored for `service`/`account`; `Ok(None)` when there is none.
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, AuthError>;

    /// Store `secret` for `service`/`account`, replacing what was there. A store that
    /// cannot be written (the default) answers [`AuthError::KeychainUnavailable`].
    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), AuthError> {
        let _ = (service, account, secret);
        Err(AuthError::KeychainUnavailable(
            "this secret store is read-only".into(),
        ))
    }
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

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), AuthError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| AuthError::Keychain(e.to_string()))?;
        entry.set_password(secret).map_err(keyring_error)
    }
}

/// "No such entry" is not an error. Neither is a keychain that cannot be asked (no Secret
/// Service on a headless box, access refused): that is [`AuthError::KeychainUnavailable`],
/// which [`api_key`] turns into "no key" plus a note. Anything else is a real error.
fn from_keyring(result: Result<String, keyring::Error>) -> Result<Option<String>, AuthError> {
    match result {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(keyring_error(e)),
    }
}

/// A keychain that cannot be asked (no Secret Service, access refused) is
/// [`AuthError::KeychainUnavailable`]; any other failure is [`AuthError::Keychain`].
fn keyring_error(e: keyring::Error) -> AuthError {
    match e {
        keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_) => {
            AuthError::KeychainUnavailable(e.to_string())
        }
        e => AuthError::Keychain(e.to_string()),
    }
}

/// The key for `provider`: the environment variable (`entry.api_key_env`, else the
/// provider's default: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`; none for
/// other providers), then the keychain (service `tautly`, account `provider`).
/// No key is `Ok` with `key: None`, also when the keychain cannot be asked at all
/// (`keychain_note` says why); only a real keychain failure is an `Err`. Surrounding whitespace is trimmed and a blank value
/// counts as unset. This reads the keychain, which can block: call it off the async
/// executor's threads.
pub fn api_key(provider: &str, entry: &ProviderEntry) -> Result<KeyLookup, AuthError> {
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
) -> Result<KeyLookup, AuthError> {
    let var = entry
        .api_key_env
        .as_deref()
        .or_else(|| default_env_var(provider));
    if let Some(name) = var
        && let Some(raw) = env(name)
    {
        let text = raw.into_string().map_err(|_| AuthError::EnvNotUnicode {
            name: name.to_string(),
        })?;
        if let Some(key) = non_blank(&text) {
            return Ok(KeyLookup {
                key: Some(key),
                keychain_note: None,
            });
        }
    }
    match store.get(KEYCHAIN_SERVICE, provider) {
        Ok(secret) => Ok(KeyLookup {
            key: secret.and_then(|s| non_blank(&s)),
            keychain_note: None,
        }),
        Err(AuthError::KeychainUnavailable(why)) => Ok(KeyLookup {
            key: None,
            keychain_note: Some(format!("keychain not used: {why}")),
        }),
        Err(e) => Err(e),
    }
}

/// What [`api_key`] found.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct KeyLookup {
    pub key: Option<String>,
    /// Set when the keychain could not be asked (and no environment key was found): why,
    /// for the caller to log next to its "no API key" message.
    pub keychain_note: Option<String>,
}

// The key never appears in logs.
impl std::fmt::Debug for KeyLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyLookup")
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("keychain_note", &self.keychain_note)
            .finish()
    }
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
        // No daemon, no session bus, store refused: nothing to ask.
        let denied = keyring::Error::NoStorageAccess("locked".into());
        assert!(matches!(
            from_keyring(Err(denied)),
            Err(AuthError::KeychainUnavailable(_))
        ));
        let no_service = keyring::Error::PlatformFailure("no bus".into());
        assert!(matches!(
            from_keyring(Err(no_service)),
            Err(AuthError::KeychainUnavailable(_))
        ));
        // Anything else is a real error.
        let bad = keyring::Error::BadEncoding(vec![0xff]);
        assert!(matches!(
            from_keyring(Err(bad)),
            Err(AuthError::Keychain(_))
        ));
    }
}
