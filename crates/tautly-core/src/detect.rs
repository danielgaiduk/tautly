//! Zero-config first run (spec 7.11.2): when no layer or flag names a model, find a
//! provider the user already has a key for (or a local Ollama) and pick its default model.
//!
//! Detection reads only the environment, the keychain and a loopback port: nothing a
//! repository controls. The configuration takes part only through the user layer's
//! catalog (`catalog_overrides`) and `providers.auto_detect`, both user-only keys.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use tautly_provider::{Catalog, ModelId, Tier};

use crate::auth::{AuthError, KEYCHAIN_SERVICE, SecretStore};
use crate::config::{LoadedConfig, OPENROUTER_KEY_ENV};
use crate::provider_setup::provider_label;

/// How long the Ollama probe may take.
pub const OLLAMA_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// The port of a local Ollama.
pub const OLLAMA_PORT: u16 = 11434;

/// The native providers and their key variables, in detection order.
const NATIVE: [(&str, &str); 3] = [
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("google", "GEMINI_API_KEY"),
];

const OPENROUTER: &str = "compat:openrouter";
const OLLAMA: &str = "compat:ollama";

/// A default model needs at least this context window.
const MIN_DEFAULT_WINDOW: u32 = 128_000;

/// Largest `/api/tags` answer read; a bigger one is not an Ollama.
const MAX_TAGS_BYTES: usize = 1024 * 1024;

/// At most this many installed models are considered, each name at most this long.
const MAX_MODELS: usize = 1000;
const MAX_MODEL_NAME: usize = 200;

/// The installed models of a local Ollama, so tests can fake one.
pub trait OllamaProbe: Send + Sync {
    /// The installed model names; `None` when no Ollama answered within `timeout`.
    fn installed_models(&self, timeout: Duration) -> Option<Vec<String>>;
}

/// `GET /api/tags` on a loopback address only (`127.0.0.1`, then `::1`), over plain
/// HTTP/1.0 with one deadline for connecting, sending and reading. It blocks for up to
/// the timeout: call it off the async executor's threads.
#[derive(Debug, Clone, Copy)]
pub struct HttpOllamaProbe {
    port: u16,
}

impl Default for HttpOllamaProbe {
    fn default() -> Self {
        HttpOllamaProbe { port: OLLAMA_PORT }
    }
}

impl HttpOllamaProbe {
    /// A probe of Ollama's port on this machine.
    pub fn new() -> Self {
        HttpOllamaProbe::default()
    }

    /// A probe of another loopback port (tests).
    pub fn on_port(port: u16) -> Self {
        HttpOllamaProbe { port }
    }

    fn get_tags(&self, deadline: Instant) -> std::io::Result<Vec<u8>> {
        let addrs = [
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, self.port)),
        ];
        let mut last = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let mut stream = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, left(deadline)?) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = e,
            }
        }
        let mut stream = stream.ok_or(last)?;
        stream.set_write_timeout(Some(left(deadline)?))?;
        // HTTP/1.0: the server closes the connection after the body and sends no chunks.
        let request = format!(
            "GET /api/tags HTTP/1.0\r\nHost: localhost:{}\r\nAccept: application/json\r\n\r\n",
            self.port
        );
        stream.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            stream.set_read_timeout(Some(left(deadline)?))?;
            let n = stream.read(&mut buf)?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            if response.len() > MAX_TAGS_BYTES {
                return Err(std::io::Error::other("answer too large"));
            }
            if complete(&response) {
                break;
            }
        }
        Ok(response)
    }
}

impl OllamaProbe for HttpOllamaProbe {
    fn installed_models(&self, timeout: Duration) -> Option<Vec<String>> {
        let response = self.get_tags(Instant::now() + timeout).ok()?;
        parse_tags(&response)
    }
}

/// The time left until `deadline`; timed out when none is.
fn left(deadline: Instant) -> std::io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(std::io::ErrorKind::TimedOut.into())
    } else {
        Ok(left)
    }
}

/// The header block and the body of an HTTP response, if the headers are complete.
fn split_response(response: &[u8]) -> Option<(&str, &[u8])> {
    let end = response.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&response[..end]).ok()?;
    Some((head, &response[end + 4..]))
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

/// Whether the whole body (by `Content-Length`) has arrived.
fn complete(response: &[u8]) -> bool {
    split_response(response)
        .and_then(|(head, body)| content_length(head).map(|n| body.len() >= n))
        .unwrap_or(false)
}

/// The model names of a `200` answer to `/api/tags` (`{"models": [{"name": …}, …]}`).
/// Names with control characters or of absurd length are dropped.
fn parse_tags(response: &[u8]) -> Option<Vec<String>> {
    let (head, body) = split_response(response)?;
    let status = head.lines().next()?.split_whitespace().nth(1)?;
    if status != "200" {
        return None;
    }
    let body = match content_length(head) {
        Some(n) => body.get(..n)?,
        None => body,
    };
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let names = value
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("name").or_else(|| m.get("model"))?.as_str())
        .filter(|n| !n.is_empty() && n.len() <= MAX_MODEL_NAME && !n.chars().any(char::is_control))
        .take(MAX_MODELS)
        .map(str::to_string)
        .collect();
    Some(names)
}

/// Where a detected provider's key came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// The environment variable of that name.
    Env(String),
    /// The keychain account of that name (service `tautly`).
    Keychain(String),
    /// A local Ollama, which needs no key.
    Ollama,
}

impl std::fmt::Display for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeySource::Env(var) => f.write_str(var),
            KeySource::Keychain(_) => f.write_str("the keychain"),
            KeySource::Ollama => f.write_str("Ollama"),
        }
    }
}

/// What detection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub model: ModelId,
    /// `anthropic`, `openai`, `google`, `compat:openrouter` or `compat:ollama`.
    pub provider: String,
    pub source: KeySource,
    /// Set when the model is not in the catalog (an unknown Ollama model).
    pub warning: Option<String>,
}

impl Detected {
    /// The line a client prints when it starts on a detected model.
    pub fn startup_line(&self) -> String {
        format!(
            "using {} (from {}) · /model to switch · tautly doctor to check",
            self.model, self.source
        )
    }
}

/// How a client should pick the session's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChoice {
    /// The configured model (named by a layer or flag, or the default with detection off).
    Configured(ModelId),
    Detected(Detected),
    /// Detection ran and found nothing.
    NotFound,
}

/// Whether provider detection applies: no layer or flag named a model and
/// `providers.auto_detect` is on.
pub fn wants_detection(loaded: &LoadedConfig) -> bool {
    !loaded.model_explicit && loaded.config.providers.auto_detect
}

/// The configured model, or (when [`wants_detection`]) what [`detect`] finds with the
/// configuration's catalog. Without detection neither `store` nor `probe` is touched.
pub fn resolve_model(
    loaded: &LoadedConfig,
    env: &dyn Fn(&str) -> Option<OsString>,
    store: &dyn SecretStore,
    probe: &dyn OllamaProbe,
) -> Result<ModelChoice, AuthError> {
    if !wants_detection(loaded) {
        return Ok(ModelChoice::Configured(loaded.config.model.clone()));
    }
    let catalog = loaded.config.effective_catalog();
    Ok(match detect(env, store, probe, &catalog)? {
        Some(d) => ModelChoice::Detected(d),
        None => ModelChoice::NotFound,
    })
}

/// The first provider with a key, in the order of spec 7.11.2: the environment variables
/// `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`; the keychain accounts
/// `anthropic`, `openai`, `google`; `OPENROUTER_API_KEY`, then the keychain account
/// `compat:openrouter`; a local Ollama with a model installed. The keychain is read only
/// when no environment key exists, and Ollama is probed only as the last resort. A
/// keychain that cannot be asked counts as empty (and is not asked again); a non-unicode
/// variable or a failing keychain is an error. This may block (keychain, probe).
pub fn detect(
    env: &dyn Fn(&str) -> Option<OsString>,
    store: &dyn SecretStore,
    probe: &dyn OllamaProbe,
    catalog: &Catalog,
) -> Result<Option<Detected>, AuthError> {
    let found = |provider: &str, source: KeySource| {
        default_model(provider, catalog).map(|model| Detected {
            model,
            provider: provider.to_string(),
            source,
            warning: None,
        })
    };
    for (provider, var) in NATIVE {
        if env_key(env, var)?
            && let Some(d) = found(provider, KeySource::Env(var.to_string()))
        {
            return Ok(Some(d));
        }
    }
    let mut keychain = Keychain {
        store,
        usable: true,
    };
    for (provider, _) in NATIVE {
        if keychain.has(provider)?
            && let Some(d) = found(provider, KeySource::Keychain(provider.to_string()))
        {
            return Ok(Some(d));
        }
    }
    if env_key(env, OPENROUTER_KEY_ENV)?
        && let Some(d) = found(OPENROUTER, KeySource::Env(OPENROUTER_KEY_ENV.to_string()))
    {
        return Ok(Some(d));
    }
    if keychain.has(OPENROUTER)?
        && let Some(d) = found(OPENROUTER, KeySource::Keychain(OPENROUTER.to_string()))
    {
        return Ok(Some(d));
    }
    Ok(probe
        .installed_models(OLLAMA_PROBE_TIMEOUT)
        .filter(|models| !models.is_empty())
        .map(|models| {
            let (model, warning) = ollama_model(&models, catalog);
            Detected {
                model,
                provider: OLLAMA.to_string(),
                source: KeySource::Ollama,
                warning,
            }
        }))
}

/// Whether `var` holds a key (set, not blank).
fn env_key(env: &dyn Fn(&str) -> Option<OsString>, var: &str) -> Result<bool, AuthError> {
    match env(var) {
        None => Ok(false),
        Some(raw) => {
            let text = raw.into_string().map_err(|_| AuthError::EnvNotUnicode {
                name: var.to_string(),
            })?;
            Ok(!text.trim().is_empty())
        }
    }
}

/// The keychain, asked until it turns out not to be available.
struct Keychain<'a> {
    store: &'a dyn SecretStore,
    usable: bool,
}

impl Keychain<'_> {
    fn has(&mut self, account: &str) -> Result<bool, AuthError> {
        if !self.usable {
            return Ok(false);
        }
        match self.store.get(KEYCHAIN_SERVICE, account) {
            Ok(secret) => Ok(secret.is_some_and(|s| !s.trim().is_empty())),
            Err(AuthError::KeychainUnavailable(_)) => {
                self.usable = false;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

/// The provider's default model: among its priced catalog models that are not
/// `tier = "small"` and have a context window of at least 128,000 tokens, the lowest input
/// price, then the lowest output price, then the id.
pub fn default_model(provider: &str, catalog: &Catalog) -> Option<ModelId> {
    catalog
        .ids()
        .filter(|id| provider_label(id) == provider)
        .filter_map(|id| {
            let info = catalog.get(&id);
            let price = info.price?;
            let eligible =
                info.tier != Some(Tier::Small) && info.caps.context_window >= MIN_DEFAULT_WINDOW;
            eligible.then_some((price.input, price.output, id))
        })
        .min_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then(a.1.total_cmp(&b.1))
                .then_with(|| a.2.as_str().cmp(b.2.as_str()))
        })
        .map(|(_, _, id)| id)
}

/// The Ollama model to use: the first `compat:ollama/…` catalog model (in catalog order)
/// that is installed; else the alphabetically first installed model, with a warning that
/// it is not in the catalog. `installed` must not be empty.
pub fn ollama_model(installed: &[String], catalog: &Catalog) -> (ModelId, Option<String>) {
    let prefix = format!("{OLLAMA}/");
    let known = catalog.ids().find(|id| {
        id.as_str()
            .strip_prefix(&prefix)
            .is_some_and(|name| installed.iter().any(|i| i == name))
    });
    if let Some(id) = known {
        return (id, None);
    }
    let first = installed.iter().min().cloned().unwrap_or_default();
    let warning = format!(
        "Ollama model `{first}` is not in the model catalog: it gets conservative \
         capabilities (32,000-token window) and no cost tracking"
    );
    (ModelId::from(format!("{prefix}{first}")), Some(warning))
}

/// What to do when no provider is usable: the variables, `tautly auth add` and Ollama.
pub fn setup_hint() -> String {
    "set ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY or OPENROUTER_API_KEY; or store a \
     key with `tautly auth add <provider>` (anthropic, openai, google, openrouter or \
     compat:<name>); or run Ollama on localhost:11434 with a model pulled. `tautly doctor` \
     checks the setup"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_answers_are_parsed_by_content_length() {
        let body = r#"{"models":[{"name":"a:1"},{"model":"b:2"},{"name":"bad\u001b"}]}"#;
        let response = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert!(complete(response.as_bytes()));
        assert!(!complete(&response.as_bytes()[..response.len() - 1]));
        assert_eq!(
            parse_tags(response.as_bytes()),
            Some(vec!["a:1".to_string(), "b:2".to_string()])
        );
        // Read to the end of the connection when no length is given.
        let open = format!("HTTP/1.0 200 OK\r\n\r\n{body}");
        assert!(!complete(open.as_bytes()));
        assert_eq!(parse_tags(open.as_bytes()).map(|v| v.len()), Some(2));
        // Not an Ollama.
        assert_eq!(parse_tags(b"HTTP/1.0 404 Not Found\r\n\r\n{}"), None);
        assert_eq!(parse_tags(b"HTTP/1.0 200 OK\r\n\r\nnot json"), None);
        assert_eq!(parse_tags(b"garbage"), None);
    }
}
