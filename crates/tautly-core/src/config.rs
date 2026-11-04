//! Layered configuration: built-in defaults, then the user file
//! (`<config home>/config.toml`), then the project file (`<root>/.tautly/config.toml`),
//! then command-line overrides. Tables merge key by key, arrays and scalars replace.
//!
//! The project file is repository content, so it is untrusted: it cannot set `providers`,
//! `telemetry`, `approve_memory`, `catalog_overrides`, `routing`, `evidence` or
//! `checkpoints` (a repository must not turn off what lets the user undo it), and in
//! `permissions` it can only lower the mode and add `deny` rules; `budget`, `max_turns`
//! and `turn_timeout_s` can only go down. Of `[mcp]` it may only declare `servers`, which
//! never merge with the user's: they are kept apart in [`Config::project_mcp_servers`] and
//! each needs the user's approval before it starts. Dropped settings come back as
//! [`LoadedConfig::warnings`]. Command-line flags are the user's own and override all.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use tautly_mcp::{McpConfig, ServerSpec};
use tautly_provider::{Catalog, ModelId};
use tautly_sandbox::{Mode, Policy, PolicyConfig};
use tautly_tools::MAX_PIN_BYTES;
use toml::{Table, Value};
use url::Url;

use crate::evidence::{EvidenceConfig, TestMatcher};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// `[evidence]`: which commands count as test runs (user layer only).
    #[serde(default)]
    pub evidence: EvidenceConfig,
    /// `[mcp]`: from the user layer, except the project's servers, which are kept apart in
    /// [`Config::project_mcp_servers`].
    #[serde(default)]
    pub mcp: McpConfig,
    /// The project layer's `[mcp.servers]`: repository-declared, so each needs the user's
    /// approval before it starts (spec 7.11.10).
    #[serde(skip)]
    pub project_mcp_servers: BTreeMap<String, ServerSpec>,
    /// `[checkpoints]`: git checkpoints of the work tree (spec 7.11.11; user layer and
    /// command line only).
    #[serde(default)]
    pub checkpoints: CheckpointConfig,
}

/// `[checkpoints]` (spec 7.11.11): snapshots of the work tree before every command and at
/// turn ends, which `/undo` and time travel restore. A snapshot that would take longer
/// than `timeout_ms`, or that finds more untracked files than the limits, is skipped with
/// a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CheckpointConfig {
    pub enabled: bool,
    /// Checkpoint refs older than this are deleted at engine start (at least 1).
    pub keep_days: u32,
    /// Longest one snapshot may take (git is killed after it; at least 1).
    pub timeout_ms: u64,
    /// Untracked (not ignored) bytes above which a snapshot is skipped, in MiB.
    pub max_untracked_mb: u64,
    /// Untracked (not ignored) files above which a snapshot is skipped.
    pub max_untracked_files: u64,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        CheckpointConfig {
            enabled: true,
            keep_days: 14,
            timeout_ms: 3_000,
            max_untracked_mb: 200,
            max_untracked_files: 20_000,
        }
    }
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
            evidence: EvidenceConfig::default(),
            mcp: McpConfig::default(),
            project_mcp_servers: BTreeMap::new(),
            checkpoints: CheckpointConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    pub anthropic: Option<ProviderEntry>,
    pub openai: Option<ProviderEntry>,
    pub google: Option<ProviderEntry>,
    /// OpenAI-compatible endpoints (`[[providers.compat]]`).
    #[serde(default)]
    pub compat: Vec<CompatEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub base_url: Option<Url>,
    /// Name of the environment variable holding the key, instead of the provider's
    /// default (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`).
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLimits {
    pub max_usd: Option<f64>,
    pub max_tokens: Option<u64>,
}

/// `[budget.turn]` and `[budget.session]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(default)]
    pub turn: BudgetLimits,
    #[serde(default)]
    pub session: BudgetLimits,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Off unless set: nothing leaves the machine by default.
    #[serde(default)]
    pub enabled: bool,
    /// OTLP endpoint; the exporter's own default when absent.
    pub endpoint: Option<Url>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// `--no-checkpoints` (`Some(false)`): sets `checkpoints.enabled`.
    pub checkpoints: Option<bool>,
}

/// Directory of the user config file: `TAUTLY_HOME` as is (tests), else
/// `$XDG_CONFIG_HOME/tautly`, else `~/.config/tautly`. Empty variables are ignored.
/// An error when the result would not be an absolute path (no `HOME`, or a relative
/// variable): a relative path would resolve against the working directory, which is
/// usually the repository, and make repository content the trusted user layer.
pub fn config_home() -> Result<PathBuf, String> {
    config_home_from(|name| std::env::var_os(name))
}

/// [`config_home`] with the environment lookup injected.
pub fn config_home_from(get: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, String> {
    let non_empty = |name: &str| get(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let absolute = |name: &str, dir: PathBuf| {
        if dir.is_absolute() {
            Ok(dir)
        } else {
            Err(format!("{name} is not an absolute path"))
        }
    };
    if let Some(home) = non_empty("TAUTLY_HOME") {
        return absolute("TAUTLY_HOME", home);
    }
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        return absolute("XDG_CONFIG_HOME", xdg).map(|d| d.join("tautly"));
    }
    match non_empty("HOME") {
        Some(home) => absolute("HOME", home).map(|d| d.join(".config").join("tautly")),
        None => Err("HOME is not set".to_string()),
    }
}

/// A loaded [`Config`] and what loading ignored.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// One line per setting the project file was not allowed to make, for the engine to
    /// report as `Event::Warning`.
    pub warnings: Vec<String>,
}

impl Config {
    /// blake3 hex of the effective configuration, for the session header (spec 6.5 H3):
    /// equal for equal configurations, whatever order the files listed keys in. It hashes
    /// the canonical JSON form: serialized through `serde_json::Value`, whose objects keep
    /// their keys sorted. It holds no secrets (keys come from the environment or the
    /// keychain, never from configuration).
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_value(self)
            .and_then(|v| serde_json::to_vec(&v))
            .unwrap_or_default();
        blake3::hash(&canonical).to_hex().to_string()
    }

    /// Defaults, then `<config home>/config.toml`, then `<root>/.tautly/config.toml`, then
    /// `cli`. A missing file is skipped. Without a usable (absolute) config home the user
    /// layer is skipped with a warning.
    pub fn load(root: &Path, cli: CliOverrides) -> Result<LoadedConfig, ConfigError> {
        Config::load_with_env(root, cli, |name| std::env::var_os(name))
    }

    /// [`Config::load`] with the environment lookup injected.
    pub fn load_with_env(
        root: &Path,
        cli: CliOverrides,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<LoadedConfig, ConfigError> {
        match config_home_from(env) {
            Ok(dir) => Config::load_from(Some(&dir.join("config.toml")), root, cli),
            Err(why) => {
                let mut loaded = Config::load_from(None, root, cli)?;
                loaded
                    .warnings
                    .insert(0, format!("user config not read: {why}"));
                Ok(loaded)
            }
        }
    }

    /// Like [`Config::load`] with an explicit user file (`None` = no user layer; a
    /// relative path is skipped with a warning, see [`config_home`]).
    ///
    /// The user file is strict: unknown keys and bad values are errors. The project file is
    /// repository content and untrusted (module docs): it is restricted before it is
    /// checked, what it may not set is dropped with a warning (unknown keys too, so a
    /// newer teammate's file does not break an older binary), and a project file that is a
    /// symlink, not a regular file or too large is skipped with a warning. It must be a
    /// plain file in a real `.tautly` directory. The user file may be a symlink (dotfile
    /// managers). Both are capped in size and never opened in a way that can block (FIFOs).
    /// A syntax error in either is an error.
    pub fn load_from(
        user_file: Option<&Path>,
        root: &Path,
        cli: CliOverrides,
    ) -> Result<LoadedConfig, ConfigError> {
        let mut warnings = Vec::new();
        let user_file = match user_file {
            Some(p) if !p.is_absolute() => {
                warnings.push(format!(
                    "user config not read: {} is not an absolute path",
                    p.display()
                ));
                None
            }
            other => other,
        };
        let project_dir = root.join(".tautly");
        let mut layers = vec![user_file.map(|p| (p.to_path_buf(), Trust::User))];
        if std::fs::symlink_metadata(&project_dir).is_ok_and(|m| m.file_type().is_symlink()) {
            warnings.push(format!(
                "{}: project config not read: it is a symbolic link",
                project_dir.display()
            ));
        } else {
            layers.push(Some((project_dir.join("config.toml"), Trust::Project)));
        }

        let mut merged = Table::new();
        let mut project_mcp_servers = BTreeMap::new();
        for (path, trust) in layers.into_iter().flatten() {
            let layer = match (read_layer(&path, trust == Trust::User), trust) {
                (Ok(layer), _) => layer,
                // Whatever the repository did to its config file (symlink, FIFO, huge,
                // `.tautly` being a file, no read permission), it must not stop startup.
                (Err(ConfigError::Unsafe { path, why }), Trust::Project) => {
                    warnings.push(format!(
                        "{}: project config not read: {why}",
                        path.display()
                    ));
                    None
                }
                (Err(ConfigError::Io { path, source }), Trust::Project) => {
                    warnings.push(format!(
                        "{}: project config not read: {source}",
                        path.display()
                    ));
                    None
                }
                (Err(e), _) => return Err(e),
            };
            let Some(mut layer) = layer else {
                continue;
            };
            if trust == Trust::Project {
                restrict_project_layer(&mut layer, &merged, &path, &mut warnings);
                project_mcp_servers = take_project_servers(&mut layer, &path, &mut warnings);
            }
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

        let mut cfg: Config = Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Invalid {
                origin: None,
                message: e.to_string(),
            })?;
        cfg.validate()?;
        cfg.project_mcp_servers = project_mcp_servers;
        Ok(LoadedConfig {
            config: cfg,
            warnings,
        })
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
        if let Some(t) = self.compaction_threshold
            && !(t.is_finite() && t > 0.0 && t <= 1.0)
        {
            return Err(format!(
                "compaction_threshold must be in (0, 1], or 0 to disable; got {t}"
            ));
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
        if let Some(m) = &self.routing.side_model
            && m.as_str().is_empty()
        {
            return Err("routing.side_model must not be empty".into());
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
        TestMatcher::new(&self.evidence).map_err(|e| format!("evidence: {e}"))?;
        if let Some(url) = &self.telemetry.endpoint {
            check_http("telemetry.endpoint", url)?;
        }
        if self.checkpoints.keep_days == 0 {
            return Err("checkpoints.keep_days must be at least 1".into());
        }
        if self.checkpoints.timeout_ms == 0 {
            return Err("checkpoints.timeout_ms must be at least 1".into());
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Trust {
    User,
    Project,
}

/// Settings a project file may not make: where requests and keys go, what leaves the
/// machine, what is written without asking, and which model data the engine believes.
const USER_ONLY_KEYS: &[&str] = &[
    "providers",
    "telemetry",
    "approve_memory",
    "catalog_overrides",
    "routing",
    // A repository could otherwise label any command a passing test (spec 7.11.7).
    "evidence",
    // A repository could otherwise turn off what undoes its commands (spec 7.11.11).
    "checkpoints",
];

/// Everything else a project file may set (some of it only to tighten).
const PROJECT_KEYS: &[&str] = &[
    "model",
    "permissions",
    "compaction_threshold",
    "max_turns",
    "turn_timeout_s",
    "budget",
    "pins",
    // Only `mcp.servers`, kept apart from the user's servers (they need approval).
    "mcp",
];

/// Repository pins are authoritative and never summarized, so they are capped like the
/// ones from AGENTS.md: each at [`MAX_PIN_BYTES`], all together at this.
const MAX_PROJECT_PINS_BYTES: usize = tautly_context::agents_md::MAX_PINNED_BYTES;

/// Most warnings one project file produces before the rest are summarized.
const MAX_PROJECT_WARNINGS: usize = 20;

/// The lowest compaction threshold a project may ask for (a floor so a repository cannot
/// make compaction thrash).
const MIN_PROJECT_THRESHOLD: f64 = 0.5;

/// Strips from a project layer everything that would loosen what the user set up, and
/// says what it dropped. This runs before the layer is checked, so nothing it drops can
/// fail the load. `so_far` is the merged defaults and user layer:
/// - unknown keys and [`USER_ONLY_KEYS`] are dropped;
/// - `permissions.mode` may only go lower than so far, `permissions.allow` is dropped and
///   `permissions.deny` rules are added to the user's (`PolicyConfig` has no `ask` rules,
///   so deny rules are the only rule a project can add);
/// - `max_turns`, `turn_timeout_s`, `budget.*.max_usd` / `max_tokens` may only go lower,
///   `compaction_threshold` only lower and within 0.5..1 (it can neither disable
///   compaction nor raise the threshold);
/// - `model` must be a priced model of the catalog the trusted layers produce, else a
///   repository could make every USD budget unenforceable;
/// - `pins` are capped.
fn restrict_project_layer(
    layer: &mut Table,
    so_far: &Table,
    path: &Path,
    warnings: &mut Vec<String>,
) {
    let first = warnings.len();
    let mut ignore = |key: &str, why: &str| {
        warnings.push(format!("{}: ignored `{key}`: {why}", path.display()));
    };

    for key in layer.keys().cloned().collect::<Vec<_>>() {
        if USER_ONLY_KEYS.contains(&key.as_str()) {
            layer.remove(&key);
            ignore(&key, "a project config may not set this");
        } else if !PROJECT_KEYS.contains(&key.as_str()) {
            layer.remove(&key);
            ignore(&key, "unknown setting");
        }
    }

    if let Some(Value::String(model)) = layer.get("model") {
        let catalog = trusted_catalog(so_far);
        let id = ModelId::from(model.as_str());
        if !(catalog.contains(&id) && catalog.get(&id).price.is_some()) {
            layer.remove("model");
            ignore(
                "model",
                "a project config may only choose a priced model from the catalog",
            );
        }
    }

    // Limits only get lower: against the default, or what the user set.
    let loosening = "a project config can only lower this limit";
    for (key, default) in [("max_turns", Some(100.0)), ("turn_timeout_s", None)] {
        if !tightens(layer, Some(so_far), key, default) {
            layer.remove(key);
            ignore(key, loosening);
        }
    }
    if let Some(wanted) = layer.get("compaction_threshold").and_then(number) {
        let current = match so_far.get("compaction_threshold").and_then(number) {
            None => 0.8,
            Some(0.0) => f64::INFINITY, // the user turned compaction off
            Some(c) => c,
        };
        if !((MIN_PROJECT_THRESHOLD..1.0).contains(&wanted) && wanted <= current) {
            layer.remove("compaction_threshold");
            ignore(
                "compaction_threshold",
                "a project config can only lower it, to between 0.5 and 1 (it cannot disable compaction)",
            );
        }
    }

    if let Some(Value::Table(budget)) = layer.get_mut("budget") {
        retain_known(budget, &["turn", "session"], "budget", &mut ignore);
        for scope in ["turn", "session"] {
            let Some(Value::Table(project)) = budget.get_mut(scope) else {
                continue;
            };
            retain_known(
                project,
                &["max_usd", "max_tokens"],
                &format!("budget.{scope}"),
                &mut ignore,
            );
            let user = so_far
                .get("budget")
                .and_then(Value::as_table)
                .and_then(|b| b.get(scope))
                .and_then(Value::as_table);
            for key in ["max_usd", "max_tokens"] {
                if !tightens(project, user, key, None) {
                    project.remove(key);
                    ignore(&format!("budget.{scope}.{key}"), loosening);
                }
            }
        }
    }

    if let Some(Value::Array(pins)) = layer.get_mut("pins") {
        // The user's pins stay (first), the project's are added after them.
        let mut all: Vec<Value> = so_far
            .get("pins")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut total = 0;
        let (mut empty, mut too_big, mut over_cap) = (0, 0, 0);
        let mut full = false;
        for pin in std::mem::take(pins) {
            let Value::String(text) = &pin else {
                all.push(pin); // a type error, for the check to report
                continue;
            };
            if all.contains(&pin) {
                continue; // an exact duplicate adds nothing
            }
            if text.trim().is_empty() {
                empty += 1;
            } else if text.len() > MAX_PIN_BYTES {
                too_big += 1;
            } else if full || total + text.len() > MAX_PROJECT_PINS_BYTES {
                full = true;
                over_cap += 1;
            } else {
                total += text.len();
                all.push(pin);
            }
        }
        *pins = all;
        let plural = |n: usize| if n == 1 { "pin" } else { "pins" };
        if empty > 0 {
            ignore(
                "pins",
                &format!("dropped {empty} empty project {}", plural(empty)),
            );
        }
        if too_big > 0 {
            ignore(
                "pins",
                &format!(
                    "dropped {too_big} project {} over the {MAX_PIN_BYTES} byte limit",
                    plural(too_big)
                ),
            );
        }
        if over_cap > 0 {
            ignore(
                "pins",
                &format!(
                    "dropped {over_cap} project {} over the {MAX_PROJECT_PINS_BYTES} byte cap",
                    plural(over_cap)
                ),
            );
        }
    }

    if let Some(Value::Table(perms)) = layer.get_mut("permissions") {
        retain_known(
            perms,
            &["mode", "allow", "deny"],
            "permissions",
            &mut ignore,
        );
        restrict_permissions(perms, so_far, &mut ignore);
    }

    match layer.get_mut("mcp") {
        Some(Value::Table(mcp)) => {
            for key in mcp.keys().cloned().collect::<Vec<_>>() {
                if key != "servers" {
                    mcp.remove(&key);
                    ignore(
                        &format!("mcp.{key}"),
                        "a project config may only declare `mcp.servers`",
                    );
                }
            }
        }
        Some(_) => {
            layer.remove("mcp");
            ignore("mcp", "not a table");
        }
        None => {}
    }

    // A hostile file can name tens of thousands of unknown keys: say so a bounded number
    // of times.
    if warnings.len() - first > MAX_PROJECT_WARNINGS {
        let more = warnings.len() - first - MAX_PROJECT_WARNINGS;
        warnings.truncate(first + MAX_PROJECT_WARNINGS);
        warnings.push(format!(
            "{}: and {more} more ignored settings",
            path.display()
        ));
    }
}

/// Take the project layer's `[mcp.servers]` out of it: they never merge with the user's
/// servers (a same-named project server must not replace the user's), and each needs an
/// approval (spec 7.11.10). A malformed one is dropped with a warning, like any other
/// project setting the loader cannot use.
fn take_project_servers(
    layer: &mut Table,
    path: &Path,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, ServerSpec> {
    let mut servers = BTreeMap::new();
    let Some(Value::Table(mut mcp)) = layer.remove("mcp") else {
        return servers;
    };
    match mcp.remove("servers") {
        Some(Value::Table(table)) => {
            let mut bad = 0;
            for (name, spec) in table {
                match spec.try_into::<ServerSpec>() {
                    Ok(spec) => {
                        servers.insert(name, spec);
                    }
                    Err(e) => {
                        bad += 1;
                        if bad <= MAX_PROJECT_WARNINGS {
                            warnings.push(format!(
                                "{}: ignored `mcp.servers.{name}`: {}",
                                path.display(),
                                e.message()
                            ));
                        }
                    }
                }
            }
            if bad > MAX_PROJECT_WARNINGS {
                warnings.push(format!(
                    "{}: and {} more ignored MCP servers",
                    path.display(),
                    bad - MAX_PROJECT_WARNINGS
                ));
            }
        }
        Some(_) => warnings.push(format!(
            "{}: ignored `mcp.servers`: not a table",
            path.display()
        )),
        None => {}
    }
    servers
}

fn restrict_permissions(perms: &mut Table, so_far: &Table, ignore: &mut impl FnMut(&str, &str)) {
    let user_perms = so_far.get("permissions").and_then(Value::as_table);

    if let Some(mode) = perms.get("mode").cloned() {
        let wanted = mode.try_into::<Mode>().ok();
        let current = user_perms
            .and_then(|p| p.get("mode"))
            .and_then(|m| m.clone().try_into::<Mode>().ok())
            .unwrap_or(Mode::Ask);
        if wanted.is_some_and(|m| strictness(m) < strictness(current)) {
            perms.remove("mode");
            ignore(
                "permissions.mode",
                "a project config can only lower the permission mode",
            );
        }
    }

    if perms
        .get("allow")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        perms.remove("allow");
        ignore(
            "permissions.allow",
            "a project config cannot add allow rules",
        );
    }

    // Arrays replace across layers, but a project must not drop the user's deny rules.
    if let Some(Value::Array(project_deny)) = perms.get_mut("deny") {
        let mut deny = user_perms
            .and_then(|p| p.get("deny"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for rule in std::mem::take(project_deny) {
            if !deny.contains(&rule) {
                deny.push(rule);
            }
        }
        *project_deny = deny;
    }
}

/// Drops (and reports) the keys of `table` not in `known`; `scope` names the table.
fn retain_known(
    table: &mut Table,
    known: &[&str],
    scope: &str,
    ignore: &mut impl FnMut(&str, &str),
) {
    for key in table.keys().cloned().collect::<Vec<_>>() {
        if !known.contains(&key.as_str()) {
            table.remove(&key);
            ignore(&format!("{scope}.{key}"), "unknown setting");
        }
    }
}

/// The built-in catalog with the trusted layers' overrides applied.
fn trusted_catalog(so_far: &Table) -> Catalog {
    let mut catalog = Catalog::builtin();
    if let Some(overrides) = so_far
        .get("catalog_overrides")
        .and_then(|v| v.clone().try_into::<Catalog>().ok())
    {
        catalog.merge(overrides);
    }
    catalog
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Whether `project[key]` is absent or at most the value `user` sets (else `default`);
/// no current value means unlimited, so any project value tightens.
fn tightens(project: &Table, user: Option<&Table>, key: &str, default: Option<f64>) -> bool {
    let Some(wanted) = project.get(key).and_then(number) else {
        return true;
    };
    let current = user.and_then(|t| t.get(key)).and_then(number).or(default);
    current.is_none_or(|c| wanted <= c)
}

/// How much a mode asks of the user: higher is stricter.
fn strictness(mode: Mode) -> u8 {
    match mode {
        Mode::Ask => 2,
        Mode::AutoEdit => 1,
        Mode::Yolo => 0,
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
    if let Some(enabled) = cli.checkpoints {
        let mut checkpoints = Table::new();
        checkpoints.insert("enabled".into(), Value::Boolean(enabled));
        t.insert("checkpoints".into(), Value::Table(checkpoints));
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
        Err(e) if !follow_symlinks && e.raw_os_error() == Some(libc::ELOOP) => {
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
