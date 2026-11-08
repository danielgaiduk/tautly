//! `tautly doctor [--json] [--offline]` (spec 7.11.2): one line per check,
//! `ok | warn | fail | info  <check>: <detail>`, and exit code 1 if any check fails.
//!
//! Keys are reported by where they come from, never by value. The network check sends
//! each key in a header (never in the URL) to its provider's model list, which costs no
//! tokens, and follows no redirect.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tautly_core::detect::{KeyVars, ModelChoice, OllamaProbe, resolve_model, setup_hint_with};
use tautly_core::{
    Approval, AuthError, CliOverrides, Config, KEYCHAIN_SERVICE, LoadedConfig, ProviderEntry,
    SecretStore, Workspaces, plan_servers, provider_label, session_root,
};

use crate::local;

/// How long one provider may take to answer the network check.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const OPENAI_URL: &str = "https://api.openai.com";
const GOOGLE_URL: &str = "https://generativelanguage.googleapis.com";

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Print the checks as one JSON object.
    #[arg(long)]
    pub json: bool,
    /// Skip contacting provider APIs (otherwise each provider with a key gets one GET of
    /// its model list, at the host the line names).
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Info,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Info => "info",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

fn check(name: &'static str, level: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        level,
        detail: detail.into(),
    }
}

/// What the checks look at.
pub struct DoctorCx<'a> {
    /// Where `tautly` would start a session (the working directory).
    pub root: &'a Path,
    /// The configuration as a session would load it, or why it does not load.
    pub loaded: Result<LoadedConfig, String>,
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
    pub store: &'a dyn SecretStore,
    /// The keychain's name for messages.
    pub keychain_name: &'a str,
    pub probe: &'a dyn OllamaProbe,
    pub data_home: Result<PathBuf, String>,
    pub offline: bool,
}

/// Run `tautly doctor`; the process exit code (1 if any check fails).
pub fn main(args: DoctorArgs) -> i32 {
    let root = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("tautly: cannot read the current directory: {e}");
            return 1;
        }
    };
    let secrets = local::secrets();
    let probe = local::ollama_probe();
    let cx = DoctorCx {
        root: &root,
        loaded: Config::load(&root, CliOverrides::default()).map_err(|e| e.to_string()),
        env: &local::env,
        store: &*secrets.store,
        keychain_name: secrets.name,
        probe: &*probe,
        data_home: tautly_core::data_home().map_err(|e| e.to_string()),
        offline: args.offline,
    };
    let checks = run(&cx);
    if args.json {
        let list: Vec<_> = checks
            .iter()
            .map(|c| json!({"name": c.name, "level": c.level.as_str(), "detail": c.detail}))
            .collect();
        let ok = !checks.iter().any(|c| c.level == Level::Fail);
        let doc = json!({"checks": list, "ok": ok});
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).unwrap_or_else(|_| doc.to_string())
        );
    } else {
        for c in &checks {
            println!(
                "{:<4}  {}: {}",
                c.level.as_str(),
                c.name,
                printable(&c.detail)
            );
        }
    }
    if checks.iter().any(|c| c.level == Level::Fail) {
        1
    } else {
        0
    }
}

/// Every check, in the order of spec 7.11.2. It may block (keychain, Ollama probe, the
/// network, git): call it off the async executor's threads.
pub fn run(cx: &DoctorCx) -> Vec<Check> {
    let mut checks = Vec::new();
    let loaded = config_check(cx, &mut checks);
    let model = model_check(cx, &loaded, &mut checks);
    let active = model.as_ref().map(provider_label);
    let keys = keys_check(cx, &loaded.config, active.as_deref(), &mut checks);
    checks.push(keychain_check(cx));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    match &runtime {
        Ok(rt) => checks.extend(rt.block_on(network_check(cx, &keys, active.as_deref()))),
        Err(e) => checks.push(check("network", Level::Fail, format!("cannot run: {e}"))),
    }
    let data_home = data_home_check(cx, &mut checks);
    let root = sandbox_check(cx, &mut checks);
    checks.push(git_check(cx, root.as_deref().unwrap_or(cx.root)));
    if let (Some(home), Some(root)) = (&data_home, &root) {
        checks.push(index_check(home, root));
        match &runtime {
            Ok(rt) => checks.extend(rt.block_on(mcp_check(&loaded.config, home, root))),
            Err(e) => checks.push(check("mcp", Level::Fail, format!("cannot run: {e}"))),
        }
    }
    checks
}

/// `config`: it loads; each of its warnings is a warn. When it does not load, the other
/// checks look at the defaults.
fn config_check(cx: &DoctorCx, checks: &mut Vec<Check>) -> LoadedConfig {
    match &cx.loaded {
        Ok(loaded) => {
            if loaded.warnings.is_empty() {
                checks.push(check("config", Level::Ok, "loaded"));
            } else {
                for w in &loaded.warnings {
                    checks.push(check("config", Level::Warn, w.clone()));
                }
            }
            loaded.clone()
        }
        Err(e) => {
            checks.push(check("config", Level::Fail, e.clone()));
            LoadedConfig {
                config: Config::default(),
                warnings: Vec::new(),
                model_explicit: false,
            }
        }
    }
}

/// `model`: the model a session would use and where it comes from; none is a fail.
fn model_check(
    cx: &DoctorCx,
    loaded: &LoadedConfig,
    checks: &mut Vec<Check>,
) -> Option<tautly_core::ModelId> {
    match resolve_model(loaded, cx.env, cx.store, cx.probe) {
        Ok(ModelChoice::Configured(model)) => {
            let from = if loaded.model_explicit {
                "from the configuration"
            } else {
                "the default; provider detection is off"
            };
            checks.push(check("model", Level::Ok, format!("{model} ({from})")));
            Some(model)
        }
        Ok(ModelChoice::Detected(d)) => {
            checks.push(check(
                "model",
                Level::Ok,
                format!("{} (detected from {})", d.model, d.source),
            ));
            if let Some(w) = d.warning {
                checks.push(check("model", Level::Warn, w));
            }
            Some(d.model)
        }
        Ok(ModelChoice::NotFound) => {
            checks.push(check(
                "model",
                Level::Fail,
                format!(
                    "no model provider found: {}",
                    setup_hint_with(&KeyVars::from_config(&loaded.config))
                ),
            ));
            None
        }
        Err(e) => {
            checks.push(check(
                "model",
                Level::Fail,
                format!("detection failed: {e}"),
            ));
            None
        }
    }
}

/// How a provider's model list is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Api {
    Anthropic,
    OpenAi,
    Google,
    Compat,
}

/// A provider whose key was found, for the network check.
struct Keyed {
    provider: String,
    api: Api,
    base_url: String,
    key: String,
}

/// `keys`: per provider, where its key comes from (environment, keychain, none).
fn keys_check(
    cx: &DoctorCx,
    config: &Config,
    active: Option<&str>,
    checks: &mut Vec<Check>,
) -> Vec<Keyed> {
    let p = &config.providers;
    let mut providers: Vec<(String, Api, ProviderEntry, Option<&str>, String)> = [
        (
            "anthropic",
            Api::Anthropic,
            &p.anthropic,
            "ANTHROPIC_API_KEY",
            ANTHROPIC_URL,
        ),
        (
            "openai",
            Api::OpenAi,
            &p.openai,
            "OPENAI_API_KEY",
            OPENAI_URL,
        ),
        (
            "google",
            Api::Google,
            &p.google,
            "GEMINI_API_KEY",
            GOOGLE_URL,
        ),
    ]
    .into_iter()
    .map(|(name, api, entry, var, url)| {
        let entry = entry.clone().unwrap_or_default();
        let base = entry
            .base_url
            .as_ref()
            .map_or(url.to_string(), |u| u.to_string());
        (name.to_string(), api, entry, Some(var), base)
    })
    .collect();
    for c in config.effective_compat() {
        let base = c.base_url.to_string();
        providers.push((
            format!("compat:{}", c.name),
            Api::Compat,
            c.provider_entry(),
            None,
            base,
        ));
    }

    let mut keychain_usable = true;
    let mut keyed = Vec::new();
    for (name, api, entry, default_var, base_url) in providers {
        let var = entry.api_key_env.as_deref().or(default_var);
        let needs_key = api != Api::Compat || var.is_some();
        let is_active = active == Some(name.as_str());
        let from_env = var.and_then(|v| {
            let value = (cx.env)(v)?.into_string().ok()?;
            let value = value.trim();
            (!value.is_empty()).then(|| (v, value.to_string()))
        });
        let (level, detail, key) = if let Some((v, key)) = from_env {
            (Level::Ok, format!("{name}: environment ({v})"), Some(key))
        } else {
            let stored = if keychain_usable {
                cx.store.get(KEYCHAIN_SERVICE, &name)
            } else {
                Ok(None)
            };
            match stored {
                Ok(Some(key)) if !key.trim().is_empty() => (
                    Level::Ok,
                    format!("{name}: keychain"),
                    Some(key.trim().to_string()),
                ),
                Ok(_) | Err(AuthError::KeychainUnavailable(_)) => {
                    if matches!(stored, Err(AuthError::KeychainUnavailable(_))) {
                        keychain_usable = false;
                    }
                    if is_active && needs_key {
                        let how = match var {
                            Some(v) => format!("set {v} or run `tautly auth add {name}`"),
                            None => format!("run `tautly auth add {name}`"),
                        };
                        (
                            Level::Fail,
                            format!("{name}: none, and the model needs one ({how})"),
                            None,
                        )
                    } else if needs_key {
                        (Level::Info, format!("{name}: none"), None)
                    } else {
                        (Level::Info, format!("{name}: none (none needed)"), None)
                    }
                }
                Err(e) => (Level::Warn, format!("{name}: {e}"), None),
            }
        };
        checks.push(check("keys", level, detail));
        if let Some(key) = key {
            keyed.push(Keyed {
                provider: name,
                api,
                base_url,
                key,
            });
        }
    }
    keyed
}

/// `keychain`: whether it can be asked.
fn keychain_check(cx: &DoctorCx) -> Check {
    match cx.store.get(KEYCHAIN_SERVICE, "anthropic") {
        Ok(_) => check(
            "keychain",
            Level::Ok,
            format!("available ({})", cx.keychain_name),
        ),
        Err(AuthError::KeychainUnavailable(why)) => check(
            "keychain",
            Level::Warn,
            format!("unavailable ({why}); keys must come from environment variables"),
        ),
        Err(e) => check("keychain", Level::Warn, e.to_string()),
    }
}

/// `network`: each provider with a key lists its models. 2xx is ok; 401/403 fails ("key
/// rejected"); anything else fails for the active model's provider and warns otherwise.
async fn network_check(cx: &DoctorCx<'_>, keyed: &[Keyed], active: Option<&str>) -> Vec<Check> {
    if cx.offline {
        return vec![check("network", Level::Info, "skipped (--offline)")];
    }
    if keyed.is_empty() {
        return vec![check(
            "network",
            Level::Info,
            "no provider has a key to check",
        )];
    }
    let client = match tautly_sandbox::http_client_builder()
        .timeout(NETWORK_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => return vec![check("network", Level::Fail, e.without_url().to_string())],
    };
    let requests: Vec<_> = keyed
        .iter()
        .map(|k| {
            let base = k.base_url.trim_end_matches('/');
            let request = match k.api {
                Api::Anthropic => client
                    .get(format!("{base}/v1/models"))
                    .header("x-api-key", &k.key)
                    .header("anthropic-version", "2023-06-01"),
                Api::OpenAi => client.get(format!("{base}/v1/models")).bearer_auth(&k.key),
                Api::Google => client
                    .get(format!("{base}/v1beta/models"))
                    .header("x-goog-api-key", &k.key),
                Api::Compat => client.get(format!("{base}/models")).bearer_auth(&k.key),
            };
            tokio::spawn(request.send())
        })
        .collect();
    let mut checks = Vec::new();
    for (k, request) in keyed.iter().zip(requests) {
        let is_active = active == Some(k.provider.as_str());
        // Says where the key went: a user `base_url` or compat entry can name any host.
        let name = format!("{} ({})", k.provider, host(&k.base_url));
        let trouble = if is_active { Level::Fail } else { Level::Warn };
        let result = match request.await {
            Ok(r) => r,
            Err(e) => {
                checks.push(check("network", trouble, format!("{name}: {e}")));
                continue;
            }
        };
        checks.push(match result {
            Ok(resp) if resp.status().is_success() => check(
                "network",
                Level::Ok,
                format!("{name}: key accepted (HTTP {})", resp.status().as_u16()),
            ),
            Ok(resp) if matches!(resp.status().as_u16(), 401 | 403) => check(
                "network",
                Level::Fail,
                format!("{name}: key rejected (HTTP {})", resp.status().as_u16()),
            ),
            Ok(resp) => check(
                "network",
                trouble,
                format!(
                    "{name}: unexpected answer (HTTP {})",
                    resp.status().as_u16()
                ),
            ),
            Err(e) if e.is_timeout() => check(
                "network",
                trouble,
                format!("{name}: no answer within {} s", NETWORK_TIMEOUT.as_secs()),
            ),
            Err(e) => check(
                "network",
                trouble,
                format!("{name}: request failed: {}", e.without_url()),
            ),
        });
    }
    checks
}

/// The host (and port, if one is given) of `url`, for messages.
fn host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| "unknown host".to_string())
}

/// `data home`: absolute and writable, else a fail.
fn data_home_check(cx: &DoctorCx, checks: &mut Vec<Check>) -> Option<PathBuf> {
    let home = match &cx.data_home {
        Ok(home) if home.is_absolute() => home.clone(),
        Ok(home) => {
            checks.push(check(
                "data home",
                Level::Fail,
                format!("{} is not an absolute path", home.display()),
            ));
            return None;
        }
        Err(e) => {
            checks.push(check("data home", Level::Fail, e.clone()));
            return None;
        }
    };
    let probe = home.join(format!(".doctor-{}", std::process::id()));
    let writable = std::fs::create_dir_all(&home).and_then(|()| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map(drop)
    });
    let _ = std::fs::remove_file(&probe);
    match writable {
        Ok(()) => {
            checks.push(check(
                "data home",
                Level::Ok,
                format!("{} (writable)", home.display()),
            ));
            Some(home)
        }
        Err(e) => {
            checks.push(check(
                "data home",
                Level::Fail,
                format!("{} is not writable: {e}", home.display()),
            ));
            None
        }
    }
}

/// `sandbox`: the session root, and that v1 has no OS isolation.
fn sandbox_check(cx: &DoctorCx, checks: &mut Vec<Check>) -> Option<PathBuf> {
    let root = match session_root(cx.root) {
        Ok(root) => {
            checks.push(check(
                "sandbox",
                Level::Ok,
                format!("root {}", root.display()),
            ));
            Some(root)
        }
        Err(e) => {
            checks.push(check("sandbox", Level::Fail, e.to_string()));
            None
        }
    };
    checks.push(check(
        "sandbox",
        Level::Info,
        "no OS isolation in v1: commands run as your user, gated by the permission policy",
    ));
    root
}

/// `git`: the binary and its version; missing is a warn (checkpoints are off). Found as
/// checkpoints find it: on `PATH` outside the work tree at `root` (CP5 review I1).
fn git_check(cx: &DoctorCx, root: &Path) -> Check {
    let found = match (cx.env)("PATH") {
        Some(path) => tautly_sandbox::which_outside_or_skipped("git", &path, root),
        None => Err(None),
    };
    let git = match found {
        Ok(git) => git,
        Err(skipped) => {
            let why = skipped.map_or(String::new(), |p| {
                format!(" (skipped {} inside the work tree)", p.display())
            });
            return check(
                "git",
                Level::Warn,
                format!("git not found on PATH{why}: checkpoints are off"),
            );
        }
    };
    match std::process::Command::new(&git).arg("--version").output() {
        Ok(out) if out.status.success() => check(
            "git",
            Level::Ok,
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ),
        Ok(out) => check(
            "git",
            Level::Warn,
            format!(
                "{} --version failed ({}): checkpoints are off",
                git.display(),
                out.status
            ),
        ),
        Err(e) => check(
            "git",
            Level::Warn,
            format!("cannot run {}: {e}: checkpoints are off", git.display()),
        ),
    }
}

/// `index`: whether the code index of this work tree exists, and how many files it holds.
/// It is neither built nor refreshed here (sessions do that in the background).
fn index_check(data_home: &Path, root: &Path) -> Check {
    let db = data_home
        .join("index")
        .join(format!("{}.db", tautly_session::paths::project_hash(root)));
    // Read-only: no migration, no index building, no write lock (a session may be
    // refreshing it right now). Whether that refresh is still going lives in the session's
    // process; what is on disk is the last committed state.
    match tautly_index::peek(&db) {
        Ok(None) => check(
            "index",
            Level::Info,
            "not built yet (sessions build it in the background)",
        ),
        Ok(Some(found)) => {
            let files = found.files;
            let plural = if files == 1 { "" } else { "s" };
            if found.is_current() {
                check(
                    "index",
                    Level::Ok,
                    format!(
                        "present, {files} file{plural} (refreshed in the background when a \
                         session starts)"
                    ),
                )
            } else {
                check(
                    "index",
                    Level::Warn,
                    format!(
                        "present, {files} file{plural}, schema version {}: a session upgrades \
                         or rebuilds it",
                        found.schema_version
                    ),
                )
            }
        }
        Err(e) => check("index", Level::Warn, format!("cannot read the index: {e}")),
    }
}

/// `mcp`: each server a session here would know about. A repository server waiting for
/// approval is a warn (Task 45's deferred check).
async fn mcp_check(config: &Config, data_home: &Path, root: &Path) -> Vec<Check> {
    if !config.mcp.enabled {
        return vec![check("mcp", Level::Info, "disabled")];
    }
    let sandbox = match Workspaces::new(data_home).get(root) {
        Ok(ws) => Arc::clone(ws.sandbox()),
        Err(e) => return vec![check("mcp", Level::Warn, e.to_string())],
    };
    let plan = plan_servers(
        &config.mcp,
        &config.project_mcp_servers,
        &[],
        false,
        data_home,
        sandbox.as_ref(),
    )
    .await;
    let mut checks: Vec<Check> = plan
        .servers
        .iter()
        .map(|s| {
            let who = format!("{} ({})", s.name, s.source);
            match s.approval {
                Approval::Pending => check(
                    "mcp",
                    Level::Warn,
                    format!(
                        "{who}: waiting for approval (asked when a session starts; headless runs \
                         need --approve-mcp)"
                    ),
                ),
                Approval::Remembered => check("mcp", Level::Ok, format!("{who}: approved")),
                _ => check("mcp", Level::Ok, format!("{who}: configured")),
            }
        })
        .collect();
    checks.extend(
        plan.warnings
            .into_iter()
            .map(|w| check("mcp", Level::Warn, w)),
    );
    if checks.is_empty() {
        checks.push(check("mcp", Level::Info, "no servers configured"));
    }
    checks
}

/// `text` with control characters escaped, so a repository's setting names cannot drive
/// the terminal.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}
