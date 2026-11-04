//! `tautly` with no `-p`, no `--rpc` and no command, on a terminal: the daily-driver REPL
//! (spec 7.11.8, `tautly-tui`).

use std::collections::HashMap;
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use tautly_core::auth::api_key_with;
use tautly_core::{Config, EngineError, provider_label};
use tautly_tui::{EngineFactory, ModelLister, ReedlineSource, ReplOptions, run_repl};
use tokio::signal::unix::{SignalKind, signal};

use crate::{Cli, Settings, local};

/// The history file under the data home.
const HISTORY_FILE: &str = "repl_history";

/// Whether this process should run the REPL: stdin and stdout are terminals.
pub fn wanted() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub async fn run(cli: &Cli, settings: Settings) -> anyhow::Result<i32> {
    let root = std::env::current_dir().context("cannot read the current directory")?;
    let resume = cli.resume_id(&settings.data_home, &root)?;
    // Detection runs (and is announced) before the session starts; a scripted provider
    // (`--mock-script`) skips it.
    let (config, settings) = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<(Config, Settings)> {
            // Its warnings are the session's to show (it loads the same files).
            let mut loaded = Config::load(&root, settings.overrides.clone())?;
            settings.detect_model(&mut loaded)?;
            Ok((loaded.config, settings))
        })
        .await??
    };
    let opts = ReplOptions {
        color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        footer: config.receipts.footer,
        history: Some(settings.data_home.join(HISTORY_FILE)),
        root: root.clone(),
        data_home: Some(settings.data_home.clone()),
        model: Some(config.model.as_str().to_string()),
        models: Some(model_lister(config, settings.secrets.clone())),
    };
    let source = ReedlineSource::new(opts.history.as_deref(), opts.color)?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let signals = futures::stream::poll_fn(move |cx| sigint.poll_recv(cx));
    run_repl(
        factory(cli, settings, root, resume),
        source,
        tokio::io::stdout(),
        signals,
        opts,
    )
    .await?;
    Ok(0)
}

/// Sessions on `root`: the first is the one `--continue` / `--resume` names (if any), and
/// each gets the start commands (`--budget-usd`).
fn factory(cli: &Cli, settings: Settings, root: PathBuf, resume: Option<String>) -> EngineFactory {
    let first = Mutex::new(resume);
    let start = cli.start_commands();
    Box::new(move |asked| {
        let resume = asked.or_else(|| first.lock().unwrap_or_else(PoisonError::into_inner).take());
        let (engine, events) = settings
            .build(&root, resume, true, None)
            .map_err(|e| EngineError::Provider(format!("{e:#}")))?;
        // Called on the blocking pool, inside the runtime.
        let handle = tokio::runtime::Handle::current();
        for cmd in start.clone() {
            let _ = handle.block_on(engine.send(cmd));
        }
        Ok((engine, events))
    })
}

/// The catalog's models whose provider has a key (environment or keychain), each provider
/// asked once per call.
fn model_lister(config: Config, secrets: local::Secrets) -> ModelLister {
    Arc::new(move || {
        let catalog = config.effective_catalog();
        let compat = config.effective_compat();
        let mut keyed: HashMap<String, bool> = HashMap::new();
        let mut out = Vec::new();
        for id in catalog.ids() {
            let label = provider_label(&id);
            let has = *keyed
                .entry(label.clone())
                .or_insert_with(|| has_key(&config, &compat, &label, &secrets));
            if has {
                out.push(id.as_str().to_string());
            }
        }
        out
    })
}

fn has_key(
    config: &Config,
    compat: &[tautly_core::CompatEntry],
    label: &str,
    secrets: &local::Secrets,
) -> bool {
    let p = &config.providers;
    let entry = match label {
        "anthropic" => p.anthropic.clone().unwrap_or_default(),
        "openai" => p.openai.clone().unwrap_or_default(),
        "google" => p.google.clone().unwrap_or_default(),
        other => match other
            .strip_prefix("compat:")
            .and_then(|n| compat.iter().find(|c| c.name == n))
        {
            Some(c) => c.provider_entry(),
            None => return false,
        },
    };
    api_key_with(label, &entry, &local::env, &*secrets.store).is_ok_and(|l| l.key.is_some())
}
