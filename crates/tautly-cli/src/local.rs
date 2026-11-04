//! What this machine provides to provider detection, `auth add` and `doctor`: the
//! environment, the OS keychain and a local Ollama.
//!
//! With the `mock` feature (tests only) two variables substitute them, so no test reads the
//! real keychain or a real Ollama: `TAUTLY_TEST_SECRET_STORE=<file>` (a JSON object
//! `{service: {account: secret}}`) and `TAUTLY_TEST_OLLAMA_PORT=<port>` (a loopback port).
//! Release builds ignore both.

use std::ffi::OsString;
use std::sync::Arc;

use tautly_core::SecretStore;
use tautly_core::detect::{HttpOllamaProbe, OllamaProbe};

/// The keychain a key is stored in, and its name for messages.
#[derive(Clone)]
pub struct Secrets {
    pub store: Arc<dyn SecretStore>,
    pub name: &'static str,
}

#[cfg(target_os = "macos")]
const KEYCHAIN_NAME: &str = "macOS Keychain";
#[cfg(not(target_os = "macos"))]
const KEYCHAIN_NAME: &str = "Secret Service keyring";

/// The OS keychain (or, in tests, the file `TAUTLY_TEST_SECRET_STORE` names).
pub fn secrets() -> Secrets {
    #[cfg(feature = "mock")]
    if let Some(path) = std::env::var_os("TAUTLY_TEST_SECRET_STORE") {
        return Secrets {
            store: Arc::new(test_store::FileStore::new(path.into())),
            name: "test secret store",
        };
    }
    Secrets {
        store: Arc::new(tautly_core::KeyringStore),
        name: KEYCHAIN_NAME,
    }
}

/// The local Ollama probe (or, in tests, one on `TAUTLY_TEST_OLLAMA_PORT`).
pub fn ollama_probe() -> Arc<dyn OllamaProbe> {
    #[cfg(feature = "mock")]
    if let Some(port) = std::env::var("TAUTLY_TEST_OLLAMA_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
    {
        return Arc::new(HttpOllamaProbe::on_port(port));
    }
    Arc::new(HttpOllamaProbe::new())
}

/// The process environment.
pub fn env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

#[cfg(feature = "mock")]
mod test_store {
    use std::path::PathBuf;

    use serde_json::{Map, Value};
    use tautly_core::{AuthError, SecretStore};

    /// A JSON file standing in for the keychain. A missing file is an empty store; a file
    /// that cannot be written is a keychain that is not available.
    pub struct FileStore {
        path: PathBuf,
    }

    impl FileStore {
        pub fn new(path: PathBuf) -> Self {
            FileStore { path }
        }

        fn read(&self) -> Result<Map<String, Value>, AuthError> {
            match std::fs::read_to_string(&self.path) {
                Ok(text) => serde_json::from_str(&text)
                    .map_err(|e| AuthError::Keychain(format!("test secret store: {e}"))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
                Err(e) => Err(AuthError::Keychain(format!("test secret store: {e}"))),
            }
        }
    }

    impl SecretStore for FileStore {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, AuthError> {
            Ok(self
                .read()?
                .get(service)
                .and_then(|accounts| accounts.get(account))
                .and_then(Value::as_str)
                .map(str::to_string))
        }

        fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), AuthError> {
            let mut all = self.read()?;
            let accounts = all
                .entry(service.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(accounts) = accounts {
                accounts.insert(account.to_string(), Value::String(secret.to_string()));
            }
            std::fs::write(&self.path, Value::Object(all).to_string())
                .map_err(|e| AuthError::KeychainUnavailable(format!("test secret store: {e}")))
        }
    }
}
