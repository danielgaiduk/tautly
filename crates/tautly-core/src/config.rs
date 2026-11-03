//! Layered configuration: built-in defaults, then the user file
//! (`<config home>/config.toml`), then the project file (`<root>/.tautly/config.toml`),
//! then command-line overrides. Tables merge key by key, arrays and scalars replace.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};
use tautly_provider::{Catalog, ModelId};
use tautly_sandbox::{Mode, Policy, PolicyConfig};
use tautly_tools::MAX_PIN_BYTES;
use toml::{Table, Value};
use url::Url;

/// Largest config file read; a bigger one is refused rather than parsed.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// Model used when no layer names one.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{}: refusing to read: {why}", path.display())]
    Unsafe { path: PathBuf, why: &'static str },
    #[error("invalid configuration{}: {message}", origin.as_ref().map(|p| format!(" in {}", p.display())).unwrap_or_default())]
    Invalid {
        /// The file the bad value came from; `None` for the merged result or CLI flags.
        origin: Option<PathBuf>,
        message: String,
    },
}

/// The effective configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_model")]
    pub model: ModelId,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default = "default_permissions", deserialize_with = "permissions_de")]
    pub permissions: PolicyConfig,
    /// Share of the context window at which compaction starts (default 0.8). In a file,
    /// `0` turns automatic compaction off, which reads back as `None`.
    #[serde(
        default = "default_compaction_threshold",
        deserialize_with = "threshold_de"
    )]
    pub compaction_threshold: Option<f32>,
    /// Provider steps per user message (default 100).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Wall-clock limit per user message; none by default.
    #[serde(default)]
    pub turn_timeout_s: Option<u64>,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Pinned items from configuration (authoritative, never summarized).
    #[serde(default)]
    pub pins: Vec<String>,
    /// Entries replacing same-id entries of the built-in model catalog
    /// (`[catalog_overrides.models."provider/model"]`).
    #[serde(default)]
    pub catalog_overrides: Option<Catalog>,
    #[serde(default)]
    pub routing: RoutingConfig,
    /// Write memory proposals without asking (default false).
    #[serde(default)]
    pub approve_memory: bool,
}

fn default_model() -> ModelId {
    ModelId::from(DEFAULT_MODEL)
}

fn default_permissions() -> PolicyConfig {
    PolicyConfig {
        mode: Mode::Ask,
        allow: Vec::new(),
        deny: Vec::new(),
    }
}

fn default_compaction_threshold() -> Option<f32> {
    Some(0.8)
}

fn default_max_turns() -> u32 {
    100
}

/// `[permissions]` with `mode` optional, so a layer can set only rules.
fn permissions_de<'de, D: Deserializer<'de>>(d: D) -> Result<PolicyConfig, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        mode: Option<Mode>,
        #[serde(default)]
        allow: Vec<String>,
        #[serde(default)]
        deny: Vec<String>,
    }
    let raw = Raw::deserialize(d)?;
    Ok(PolicyConfig {
        mode: raw.mode.unwrap_or(Mode::Ask),
        allow: raw.allow,
        deny: raw.deny,
    })
}

fn threshold_de<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    let value = f32::deserialize(d)?;
    Ok((value != 0.0).then_some(value))
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: default_model(),
            providers: ProvidersConfig::default(),
            permissions: default_permissions(),
            compaction_threshold: default_compaction_threshold(),
            max_turns: default_max_turns(),
            turn_timeout_s: None,
            budget: BudgetConfig::default(),
            telemetry: TelemetryConfig::default(),
            pins: Vec::new(),
            catalog_overrides: None,
            routing: RoutingConfig::default(),
            approve_memory: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    pub anthropic: Option<ProviderEntry>,
    pub openai: Option<ProviderEntry>,
    pub google: Option<ProviderEntry>,
    /// OpenAI-compatible endpoints (`[[providers.compat]]`).
    #[serde(default)]
    pub compat: Vec<CompatEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub base_url: Option<Url>,
    /// Name of the environment variable holding the key, instead of the provider's
    /// default (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`).
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatEntry {
    /// Selects the endpoint in a model id: `compat:<name>/model`.
    pub name: String,
    pub base_url: Url,
    pub api_key_env: Option<String>,
}

impl CompatEntry {
    /// The entry in the shape [`crate::auth::api_key`] takes.
    pub fn provider_entry(&self) -> ProviderEntry {
        ProviderEntry {
            base_url: Some(self.base_url.clone()),
            api_key_env: self.api_key_env.clone(),
        }
    }
}

/// A spending limit; `None` = unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLimits {
    pub max_usd: Option<f64>,
    pub max_tokens: Option<u64>,
}

/// `[budget.turn]` and `[budget.session]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(default)]
    pub turn: BudgetLimits,
    #[serde(default)]
    pub session: BudgetLimits,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Off unless set: nothing leaves the machine by default.
    #[serde(default)]
    pub enabled: bool,
    /// OTLP endpoint; the exporter's own default when absent.
    pub endpoint: Option<Url>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// Model for compaction summaries, memory work and `web_fetch` questions; the
    /// catalog's small model of the main provider when absent.
    pub side_model: Option<ModelId>,
}

/// Command-line flags that override files.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub model: Option<ModelId>,
    /// `--approve auto-edit|yolo`.
    pub mode: Option<Mode>,
    /// `--approve-memory`.
    pub approve_memory: Option<bool>,
}

/// Directory of the user config file: `TAUTLY_HOME` as is (tests), else
/// `$XDG_CONFIG_HOME/tautly`, else `~/.config/tautly`. Empty variables are ignored.
pub fn config_home() -> PathBuf {
    config_home_from(|name| std::env::var_os(name))
}

/// [`config_home`] with the environment lookup injected.
pub fn config_home_from(get: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let non_empty = |name: &str| get(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(home) = non_empty("TAUTLY_HOME") {
        return home;
    }
    non_empty("XDG_CONFIG_HOME")
        .or_else(|| non_empty("HOME").map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("tautly")
}

impl Config {
    /// Defaults, then `<config home>/config.toml`, then `<root>/.tautly/config.toml`, then
    /// `cli`. A missing file is skipped.
    pub fn load(root: &Path, cli: CliOverrides) -> Result<Config, ConfigError> {
        Config::load_from(Some(&config_home().join("config.toml")), root, cli)
    }

    /// Like [`Config::load`] with an explicit user file (`None` = no user layer).
    ///
    /// The project file is repository content, so it must be a plain file in place (not a
    /// symlink); the user file may be a symlink (dotfile managers). Both are capped in size
    /// and never opened in a way that can block (FIFOs).
    pub fn load_from(
        user_file: Option<&Path>,
        root: &Path,
        cli: CliOverrides,
    ) -> Result<Config, ConfigError> {
        let project_file = root.join(".tautly").join("config.toml");
        let layers = [
            user_file.map(|p| (p.to_path_buf(), true)),
            Some((project_file, false)),
        ];

        let mut merged = Table::new();
        for (path, follow_symlinks) in layers.into_iter().flatten() {
            let Some(layer) = read_layer(&path, follow_symlinks)? else {
                continue;
            };
            // Check the layer alone so a bad value names the file it came from.
            Value::Table(layer.clone())
                .try_into::<Config>()
                .map_err(|e| ConfigError::Invalid {
                    origin: Some(path.clone()),
                    message: e.to_string(),
                })
                .and_then(|cfg| cfg.check(Some(&path)))?;
            deep_merge(&mut merged, layer);
        }
        deep_merge(&mut merged, cli_table(&cli));

        let cfg: Config = Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Invalid {
                origin: None,
                message: e.to_string(),
            })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Rejects values no layer should set: out-of-range numbers, non-http(s) URLs,
    /// malformed permission rules, duplicate or unusable provider names.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.check(None)
    }

    fn check(&self, origin: Option<&Path>) -> Result<(), ConfigError> {
        self.problem().map_err(|message| ConfigError::Invalid {
            origin: origin.map(Path::to_path_buf),
            message,
        })
    }

    fn problem(&self) -> Result<(), String> {
        if self.model.as_str().is_empty() {
            return Err("model must not be empty".into());
        }
        if let Some(t) = self.compaction_threshold {
            if !(t.is_finite() && t > 0.0 && t <= 1.0) {
                return Err(format!(
                    "compaction_threshold must be in (0, 1], or 0 to disable; got {t}"
                ));
            }
        }
        if self.max_turns == 0 {
            return Err("max_turns must be at least 1".into());
        }
        if self.turn_timeout_s == Some(0) {
            return Err("turn_timeout_s must be at least 1".into());
        }
        for (scope, limits) in [
            ("turn", &self.budget.turn),
            ("session", &self.budget.session),
        ] {
            if limits.max_usd.is_some_and(|v| !(v.is_finite() && v > 0.0)) {
                return Err(format!("budget.{scope}.max_usd must be a positive number"));
            }
            if limits.max_tokens == Some(0) {
                return Err(format!("budget.{scope}.max_tokens must be at least 1"));
            }
        }
        if let Some(m) = &self.routing.side_model {
            if m.as_str().is_empty() {
                return Err("routing.side_model must not be empty".into());
            }
        }
        for pin in &self.pins {
            if pin.trim().is_empty() {
                return Err("pins must not be empty".into());
            }
            if pin.len() > MAX_PIN_BYTES {
                return Err(format!("a pin holds at most {MAX_PIN_BYTES} bytes"));
            }
        }
        Policy::new(self.permissions.clone(), PathBuf::new()).map_err(|e| e.to_string())?;
        if let Some(url) = &self.telemetry.endpoint {
            check_http("telemetry.endpoint", url)?;
        }

        let p = &self.providers;
        for (name, entry) in [
            ("anthropic", &p.anthropic),
            ("openai", &p.openai),
            ("google", &p.google),
        ] {
            if let Some(e) = entry {
                if let Some(url) = &e.base_url {
                    check_http(&format!("providers.{name}.base_url"), url)?;
                }
                check_env_name(&format!("providers.{name}.api_key_env"), &e.api_key_env)?;
            }
        }
        let mut seen = HashSet::new();
        for c in &p.compat {
            let ok = !c.name.is_empty()
                && c.name
                    .chars()
                    .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'));
            if !ok {
                return Err(format!(
                    "providers.compat name {:?} must be non-empty letters, digits, '-', '_' or '.'",
                    c.name
                ));
            }
            if !seen.insert(c.name.as_str()) {
                return Err(format!("providers.compat name {:?} appears twice", c.name));
            }
            check_http(
                &format!("providers.compat[{}].base_url", c.name),
                &c.base_url,
            )?;
            check_env_name(
                &format!("providers.compat[{}].api_key_env", c.name),
                &c.api_key_env,
            )?;
        }
        Ok(())
    }
}

fn check_http(what: &str, url: &Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!("{what} must be an http(s) URL, not {other}://")),
    }
}

fn check_env_name(what: &str, name: &Option<String>) -> Result<(), String> {
    match name {
        Some(n) if n.is_empty() || n.contains('=') || n.contains('\0') => {
            Err(format!("{what} is not a valid environment variable name"))
        }
        _ => Ok(()),
    }
}

/// Tables merge key by key (recursively); anything else, arrays included, is replaced.
fn deep_merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(b)), Value::Table(o)) => deep_merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn cli_table(cli: &CliOverrides) -> Table {
    let mut t = Table::new();
    if let Some(model) = &cli.model {
        t.insert("model".into(), Value::String(model.as_str().to_string()));
    }
    if let Some(mode) = cli.mode {
        let mut permissions = Table::new();
        if let Ok(v) = Value::try_from(mode) {
            permissions.insert("mode".into(), v);
        }
        t.insert("permissions".into(), Value::Table(permissions));
    }
    if let Some(approve) = cli.approve_memory {
        t.insert("approve_memory".into(), Value::Boolean(approve));
    }
    t
}

/// The file's table; `None` when it does not exist.
fn read_layer(path: &Path, follow_symlinks: bool) -> Result<Option<Table>, ConfigError> {
    let io = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    let unsafe_file = |why| ConfigError::Unsafe {
        path: path.to_path_buf(),
        why,
    };

    // O_NONBLOCK: opening a FIFO must not wait for a writer.
    let mut flags = libc::O_NONBLOCK;
    if !follow_symlinks {
        flags |= libc::O_NOFOLLOW;
    }
    let file = match File::options().read(true).custom_flags(flags).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(unsafe_file("it is a symbolic link"));
        }
        Err(e) => return Err(io(e)),
    };
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(unsafe_file("it is not a regular file"));
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(unsafe_file("it is larger than 256 KiB"));
    }
    let mut text = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(io)?;
    toml::from_str(&text)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
}
