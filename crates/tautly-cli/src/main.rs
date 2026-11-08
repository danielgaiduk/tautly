//! `tautly`: the command-line entry point (spec 3.4).
//!
//! - `tautly -p PROMPT [--output text|json]` runs one prompt headless and exits with a
//!   code from how the turn ended (0 completed, 1 error, 2 budget / max_turns / timeout,
//!   130 cancelled by Ctrl-C); after SIGTERM or SIGHUP, 128 + the signal (143, 129).
//! - `tautly --rpc` serves the JSON-RPC protocol on stdin/stdout until the client sends
//!   `shutdown` or disconnects (exit 0), or SIGTERM / SIGHUP arrives (143 / 129, after
//!   every session has shut down).
//! - `tautly` alone, on a terminal, runs the daily-driver REPL (spec 7.11.8): messages,
//!   approvals (`y`, `n`, `a` for this session), slash commands, a receipt after each turn.
//! - `tautly acp` serves the Agent Client Protocol to an editor on stdin/stdout (spec
//!   7.11.9) until the editor disconnects (exit 0), or SIGTERM / SIGHUP (143 / 129).
//! - `tautly auth add <provider>` stores an API key in the OS keychain; `tautly doctor`
//!   checks the setup (spec 7.11.2).
//!
//! `tautly prompt|sessions|models|fork|undo|memory|index|mcp ...` are utility commands
//! (`cmd`): they read or change what sessions leave behind and exit.
//!
//! Flags shared by `-p`, `--rpc`, `acp` (written before it) and the REPL: `--approve
//! auto-edit|yolo` (headless `ask` is otherwise denied), `--approve-memory`,
//! `--approve-mcp`, `--model ID`, `--budget-usd N` (a session budget, sent before the
//! first message), `--continue` / `--resume ID` (not with `acp`), `--no-checkpoints`.
//! Command-line flags are trusted: they override every configuration file (spec 2.2).
//!
//! When no configuration layer or flag names a model, the provider is detected (spec
//! 7.11.2): the first key in the environment or the keychain, else a local Ollama. The
//! choice is announced on stderr; nothing found is an error with a setup hint.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};
use tautly_acp::{Bridge, ModelChoices, ModelLister, shared_factory};
use tautly_core::EngineError;
use tautly_core::detect::{
    Detected, KeyVars, OllamaProbe, detect_with, setup_hint_with, wants_detection,
};
use tautly_core::{
    BudgetScope, CliOverrides, Command, Config, Engine, EngineBuilder, Event, LoadedConfig, Mode,
    ModelId, ProviderSetupError, Workspaces, hold_signal_router, install_signal_handlers,
    install_termination_handlers, provider_for_with,
};
use tautly_rpc::{EngineFactory, OutputFormat, Server, run_oneshot};
use tokio::io::BufReader;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

mod cmd;
mod local;
mod repl;

/// How long a session still shutting down when the client is done may hold up the exit.
const EXIT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(
    name = "tautly",
    version,
    about = "A token-efficient AI coding agent",
    after_help = "Exit codes with -p: 0 completed, 1 error, 2 budget/max turns/timeout, \
                  130 cancelled (Ctrl-C), 143 SIGTERM, 129 SIGHUP."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Sub>,

    /// Run PROMPT as one headless turn, print what happens and exit.
    #[arg(
        short = 'p',
        long = "prompt",
        value_name = "PROMPT",
        conflicts_with = "rpc"
    )]
    prompt: Option<String>,

    /// What -p prints: the model's text (default), or every event as one JSON object per
    /// line.
    #[arg(long, value_enum, value_name = "FORMAT")]
    output: Option<Output>,

    /// Allow without asking: edits in the workspace (auto-edit) or everything that no
    /// deny rule or protected path stops (yolo). Headless, anything that would ask is
    /// denied otherwise.
    #[arg(long, value_enum, value_name = "MODE")]
    approve: Option<Approve>,

    /// The model, as <provider>/<model> (provider anthropic, openai, google or
    /// compat:<name>).
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Stop the session once it has cost this many US dollars.
    #[arg(long, value_name = "USD", value_parser = parse_usd)]
    budget_usd: Option<f64>,

    /// Save memory the agent proposes without asking (headless, it is rejected otherwise).
    #[arg(long)]
    approve_memory: bool,

    /// Start MCP servers the repository declares that you have not approved, for this
    /// session only.
    #[arg(long)]
    approve_mcp: bool,

    /// Continue the latest session of this work tree.
    #[arg(short = 'c', long = "continue", conflicts_with = "resume")]
    continue_latest: bool,

    /// Resume session ID of this work tree.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,

    /// Serve the JSON-RPC protocol on stdin and stdout.
    #[arg(long)]
    rpc: bool,

    /// Take no git checkpoints before commands (undo then reverts only the agent's edits).
    #[arg(long)]
    no_checkpoints: bool,

    /// Do not print the receipt footer after each turn of the REPL.
    #[arg(long)]
    no_footer: bool,

    /// Use a scripted provider: a JSON array of responses, each an array of provider
    /// events (tests).
    #[cfg(feature = "mock")]
    #[arg(long, value_name = "FILE", hide = true)]
    mock_script: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Sub {
    /// Store API keys in the OS keychain.
    #[command(subcommand)]
    Auth(cmd::auth::AuthCmd),
    /// Check the configuration, model, keys, network, data directory, sandbox, git, code
    /// index and MCP servers.
    Doctor(cmd::doctor::DoctorArgs),
    /// Serve the Agent Client Protocol on stdin and stdout, for editors (Zed: an
    /// `agent_servers` entry running `tautly acp`).
    Acp,
    #[command(flatten)]
    Util(cmd::Command),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Output {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Approve {
    AutoEdit,
    Yolo,
}

fn parse_usd(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok(v),
        _ => Err(format!("`{s}` is not an amount of US dollars")),
    }
}

/// A model id must name one of the providers Tautly has an adapter for (core decides).
fn check_model(id: &str) -> Result<(), String> {
    tautly_core::known_provider(&ModelId::from(id)).map_err(|e| format!("{e}: {}", e.hint()))
}

/// A provider that cannot be set up. A missing key gets the same hint as a first run that
/// found no provider (spec 7.11.2), naming the variables `config` makes the session read.
fn setup_error(e: ProviderSetupError, config: &Config) -> anyhow::Error {
    match e {
        ProviderSetupError::MissingKey { .. } => {
            let hint = setup_hint_with(&KeyVars::from_config(config));
            anyhow!("{e}\nhint: {hint}")
        }
        other => anyhow!("{other}: {}", other.hint()),
    }
}

/// The first SIGTERM or SIGHUP, as the exit code it means (128 + its number). The signals
/// are watched from this call on (alongside the engines' own handlers, which shut the
/// sessions down).
fn terminated() -> std::io::Result<impl Future<Output = i32> + Send + 'static> {
    const SIGHUP: i32 = 1;
    const SIGTERM: i32 = 15;
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    Ok(async move {
        tokio::select! {
            Some(()) = term.recv() => 128 + SIGTERM,
            Some(()) = hup.recv() => 128 + SIGHUP,
            else => std::future::pending().await,
        }
    })
}

impl Cli {
    fn overrides(&self) -> CliOverrides {
        CliOverrides {
            model: self.model.as_deref().map(ModelId::from),
            mode: self.approve.map(|a| match a {
                Approve::AutoEdit => Mode::AutoEdit,
                Approve::Yolo => Mode::Yolo,
            }),
            approve_memory: self.approve_memory.then_some(true),
            checkpoints: self.no_checkpoints.then_some(false),
            receipts_footer: self.no_footer.then_some(false),
        }
    }

    /// Commands every session gets before the first message.
    fn start_commands(&self) -> Vec<Command> {
        self.budget_usd
            .map(|usd| Command::SetBudget {
                scope: BudgetScope::Session,
                max_usd: Some(usd),
                max_tokens: None,
            })
            .into_iter()
            .collect()
    }

    /// The session to resume in the work tree of `root`, if any.
    fn resume_id(&self, data_home: &Path, root: &Path) -> anyhow::Result<Option<String>> {
        if let Some(id) = &self.resume {
            return Ok(Some(id.clone()));
        }
        if !self.continue_latest {
            return Ok(None);
        }
        match tautly_core::sessions::latest_here(data_home, root)? {
            Some(id) => Ok(Some(id)),
            None => bail!("there is no earlier session in this work tree to continue"),
        }
    }
}

/// What every session of this process is built with.
#[derive(Clone)]
struct Settings {
    overrides: CliOverrides,
    data_home: PathBuf,
    approve_mcp: bool,
    secrets: local::Secrets,
    probe: Arc<dyn OllamaProbe>,
    /// What provider detection found, once it found something (spec 7.11.2): the same for
    /// every later session of the process, so it is announced once. Nothing found, or an
    /// error, is not kept: the next session detects again (after `tautly auth add`, say).
    detected: Arc<OnceLock<Detected>>,
    /// The REPL owns Ctrl-C: the engines' signal handler covers SIGTERM and SIGHUP only.
    repl: bool,
    #[cfg(feature = "mock")]
    provider: Option<Arc<dyn tautly_provider::Provider>>,
}

impl Settings {
    fn new(cli: &Cli) -> anyhow::Result<Settings> {
        #[cfg(feature = "mock")]
        let provider = cli.mock_script.as_deref().map(mock::provider).transpose()?;
        #[cfg(feature = "mock")]
        let scripted = provider.is_some();
        #[cfg(not(feature = "mock"))]
        let scripted = false;
        if let (Some(model), false) = (&cli.model, scripted) {
            check_model(model).map_err(anyhow::Error::msg)?;
        }
        Ok(Settings {
            overrides: cli.overrides(),
            data_home: tautly_core::data_home().context("cannot find the data directory")?,
            approve_mcp: cli.approve_mcp,
            secrets: local::secrets(),
            probe: local::ollama_probe(),
            detected: Arc::new(OnceLock::new()),
            repl: false,
            #[cfg(feature = "mock")]
            provider,
        })
    }

    /// Whether a scripted provider replaces the real one (`--mock-script`): then nothing
    /// is detected and no key is needed.
    fn scripted(&self) -> bool {
        #[cfg(feature = "mock")]
        return self.provider.is_some();
        #[cfg(not(feature = "mock"))]
        false
    }

    /// Why `model` cannot be set up for a session on `root` now because its provider has no
    /// key (with the setup hint), if so; nothing for a scripted provider or another problem
    /// (which the session reports itself). This may read the keychain (blocking).
    fn key_problem(&self, root: &Path, model: &ModelId) -> Option<String> {
        if self.scripted() {
            return None;
        }
        let config = Config::load(root, self.overrides.clone()).ok()?.config;
        match provider_for_with(
            &config,
            model,
            Arc::new(config.effective_catalog()),
            &local::env,
            &*self.secrets.store,
        ) {
            Err(e @ ProviderSetupError::MissingKey { .. }) => Some(format!(
                "{e} (hint: {})",
                setup_hint_with(&KeyVars::from_config(&config))
            )),
            _ => None,
        }
    }

    /// Set the model when no layer or flag names one and detection is on, announcing it on
    /// stderr the first time. Detection reads the key variables the configuration names
    /// (as the session will). Nothing found is an error with the setup hint. This may read
    /// the keychain and wait for the Ollama probe (blocking).
    fn detect_model(&self, loaded: &mut LoadedConfig) -> anyhow::Result<()> {
        if self.scripted() || !wants_detection(loaded) {
            return Ok(());
        }
        let vars = KeyVars::from_config(&loaded.config);
        let found = match self.detected.get() {
            Some(d) => d.clone(),
            None => {
                let catalog = loaded.config.effective_catalog();
                let found = detect_with(
                    &local::env,
                    &*self.secrets.store,
                    &*self.probe,
                    &catalog,
                    &vars,
                )
                .map_err(|e| anyhow!("provider detection failed: {e}"))?;
                let Some(d) = found else {
                    bail!("no model provider found\nhint: {}", setup_hint_with(&vars));
                };
                // Two sessions may detect at once; the first to finish announces.
                if self.detected.set(d.clone()).is_ok() {
                    eprintln!("{}", d.startup_line());
                }
                d
            }
        };
        loaded.config.model = found.model;
        // An uncatalogued Ollama model: the session reports it as a warning.
        loaded.warnings.extend(found.warning);
        Ok(())
    }

    /// A session on `root` (this may block: configuration files, the keychain).
    fn build(
        &self,
        root: &Path,
        resume: Option<String>,
        interactive: bool,
        workspace: Option<Arc<tautly_core::Workspace>>,
    ) -> anyhow::Result<(Engine, mpsc::Receiver<Event>)> {
        self.build_with(root, resume, interactive, workspace, Vec::new())
    }

    /// [`build`](Self::build) with MCP servers the client passed (an editor over ACP),
    /// trusted like the user's own.
    fn build_with(
        &self,
        root: &Path,
        resume: Option<String>,
        interactive: bool,
        workspace: Option<Arc<tautly_core::Workspace>>,
        client_mcp: Vec<(String, tautly_core::ServerSpec)>,
    ) -> anyhow::Result<(Engine, mpsc::Receiver<Event>)> {
        let mut loaded = Config::load(root, self.overrides.clone())?;
        self.detect_model(&mut loaded)?;
        let mut builder = EngineBuilder {
            resume,
            interactive,
            workspace,
            client_mcp,
            approve_mcp: self.approve_mcp,
            data_home: Some(self.data_home.clone()),
            #[cfg(feature = "mock")]
            provider: self.provider.clone(),
            ..EngineBuilder::from_loaded(loaded, root)
        };
        // Every provider the engine builds (side model, `SetModel`) reads this process's
        // keychain (in tests, the file standing in for it).
        builder.secret_store = Some(Arc::clone(&self.secrets.store));
        if !self.scripted() {
            // The main provider is set up here so a missing key fails before the session
            // starts, with the setup hint.
            let config = &builder.config;
            let provider = provider_for_with(
                config,
                &config.model,
                Arc::new(config.effective_catalog()),
                &local::env,
                &*self.secrets.store,
            )
            .map_err(|e| setup_error(e, config))?;
            builder.provider = Some(provider);
        }
        let (engine, events) = builder.build()?;
        let installed = if self.repl {
            install_termination_handlers(&engine)
        } else {
            install_signal_handlers(&engine)
        };
        if let Err(e) = installed {
            eprintln!("tautly: signals are not handled: {e}");
        }
        Ok((engine, events))
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    if cli.command.is_some()
        && (cli.prompt.is_some() || cli.rpc || cli.continue_latest || cli.resume.is_some())
    {
        Cli::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "-p, --rpc, --continue and --resume do not go with a command",
            )
            .exit();
    }
    match cli.command.take() {
        Some(Sub::Auth(cmd)) => return exit_code(cmd::auth::main(cmd)),
        Some(Sub::Doctor(args)) => return exit_code(cmd::doctor::main(args)),
        other => cli.command = other,
    }
    if cli.output.is_some() && cli.prompt.is_none() {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--output applies to -p PROMPT only",
            )
            .exit();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tautly: cannot start: {e}");
            return ExitCode::from(1);
        }
    };
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            // A configuration error quotes the repository's file (CP5 review M1).
            eprintln!(
                "tautly: {}",
                tautly_core::printable_multiline(&format!("{e:#}"))
            );
            1
        }
    };
    // Normally every session has shut down (its event stream ended, its log closed) and
    // nothing is worth waiting for: what is left is blocking work such as the stdin reader
    // of `--rpc`/`acp` or a file watcher still starting. Only a session still closing (an
    // error path) gets the grace.
    if tautly_core::sessions_running() {
        runtime.shutdown_timeout(EXIT_GRACE);
    } else {
        runtime.shutdown_background();
    }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn run(mut cli: Cli) -> anyhow::Result<i32> {
    match cli.command.take() {
        Some(Sub::Util(command)) => return cmd::run(command, &cli).await,
        Some(Sub::Acp) => return acp(&cli).await,
        _ => {}
    }
    if cli.prompt.is_none() && !cli.rpc {
        if !repl::wanted() {
            eprintln!("tautly: the REPL needs a terminal; use -p PROMPT or --rpc (see --help)");
            return Ok(2);
        }
        let settings = Settings::new(&cli)?;
        return repl::run(&cli, settings).await;
    }
    let settings = Settings::new(&cli)?;
    let root = std::env::current_dir().context("cannot read the current directory")?;
    let resume = cli.resume_id(&settings.data_home, &root)?;
    match &cli.prompt {
        Some(prompt) => oneshot(&cli, settings, root, resume, prompt.clone()).await,
        None => rpc(&cli, settings, root, resume).await,
    }
}

async fn oneshot(
    cli: &Cli,
    settings: Settings,
    root: PathBuf,
    resume: Option<String>,
    prompt: String,
) -> anyhow::Result<i32> {
    let output = output_format(cli.output);
    headless(settings, root, resume, prompt, cli.start_commands(), output).await
}

fn output_format(output: Option<Output>) -> OutputFormat {
    match output {
        Some(Output::Json) => OutputFormat::Json,
        Some(Output::Text) | None => OutputFormat::Text,
    }
}

/// Run `prompt` as one headless turn of a session on `root` (`tautly -p`, `tautly fork
/// --prompt`), after the `start` commands; the exit code (see the module docs).
async fn headless(
    settings: Settings,
    root: PathBuf,
    resume: Option<String>,
    prompt: String,
    start: Vec<Command>,
    output: OutputFormat,
) -> anyhow::Result<i32> {
    // Watched before the engine (and its own handlers) exists, so a SIGTERM or SIGHUP at
    // any point still ends the process with its code.
    let terminated = tokio::spawn(terminated()?);
    let (engine, mut events) =
        tokio::task::spawn_blocking(move || settings.build(&root, resume, false, None)).await??;
    if terminated.is_finished() {
        // The signal came before the session could hear it: shut the session down unused.
        let _ = engine.send(Command::Shutdown).await;
        drop(engine);
        while events.recv().await.is_some() {}
        return Ok(terminated.await.unwrap_or(1));
    }
    for cmd in start {
        let _ = engine.send(cmd).await;
    }
    // SIGTERM / SIGHUP shut the session down (the turn ends as cancelled); the exit code
    // still says which signal it was.
    let code = run_oneshot(engine, events, prompt, output, tokio::io::stdout()).await;
    if terminated.is_finished() {
        return Ok(terminated.await.unwrap_or(code));
    }
    terminated.abort();
    Ok(code)
}

async fn rpc(
    cli: &Cli,
    settings: Settings,
    root: PathBuf,
    resume: Option<String>,
) -> anyhow::Result<i32> {
    let workspaces = Workspaces::new(settings.data_home.clone());
    let factory: EngineFactory = Arc::new(move |spec| {
        settings
            .build(
                &spec.root,
                spec.resume,
                spec.interactive,
                Some(spec.workspace),
            )
            .map_err(|e| format!("{e:#}").into())
    });
    // SIGTERM / SIGHUP: the engines' handlers shut the sessions down, and the server stops
    // as on a disconnect, exiting with the signal's code once they have.
    let signalled = Arc::new(AtomicI32::new(0));
    let terminated = terminated()?;
    let _hold = hold_signal_router()?;
    let stop = {
        let signalled = Arc::clone(&signalled);
        async move { signalled.store(terminated.await, Ordering::SeqCst) }
    };
    let served = Server::new(factory, workspaces, root)
        .resume(resume)
        .start_commands(cli.start_commands())
        .stop_on(stop)
        .serve(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
        .await;
    match served {
        Ok(()) => match signalled.load(Ordering::SeqCst) {
            0 => Ok(0),
            code => Ok(code),
        },
        Err(e) => {
            eprintln!("tautly: {e}");
            Ok(1)
        }
    }
}

/// `tautly acp` (spec 7.11.9): editors drive sessions over the Agent Client Protocol.
/// Every session is interactive (the editor answers approvals), on the git top level of
/// the editor's `cwd`, sharing one workspace per root; stdout carries only protocol
/// frames. Exits 0 at the end of input, or 143 / 129 after SIGTERM / SIGHUP.
async fn acp(cli: &Cli) -> anyhow::Result<i32> {
    let settings = Settings::new(cli)?;
    let workspaces = Workspaces::new(settings.data_home.clone());
    let models: ModelLister = {
        let settings = settings.clone();
        Arc::new(move |root: &Path| {
            let mut loaded = Config::load(root, settings.overrides.clone()).ok()?;
            settings.detect_model(&mut loaded).ok()?;
            let current = loaded.config.model.as_str().to_string();
            let available = repl::model_lister(loaded.config, settings.secrets.clone())();
            Some(ModelChoices { current, available })
        })
    };
    let start = cli.start_commands();
    let data_home = settings.data_home.clone();
    let factory = shared_factory(workspaces, move |root, resume, mcp, workspace| {
        let (engine, events) = settings
            .build_with(&root, resume, true, Some(workspace), mcp)
            .map_err(|e| EngineError::Provider(format!("{e:#}")))?;
        // Called on the blocking pool, inside the runtime.
        let handle = tokio::runtime::Handle::current();
        for cmd in start.clone() {
            let _ = handle.block_on(engine.send(cmd));
        }
        Ok((engine, events))
    });
    let signalled = Arc::new(AtomicI32::new(0));
    let terminated = terminated()?;
    let _hold = hold_signal_router()?;
    let stop = {
        let signalled = Arc::clone(&signalled);
        async move { signalled.store(terminated.await, Ordering::SeqCst) }
    };
    let served = Bridge::new(factory)
        .models(models)
        .data_home(data_home)
        .stop_on(stop)
        .serve(tokio::io::stdin(), tokio::io::stdout())
        .await;
    match served {
        Ok(()) => Ok(signalled.load(Ordering::SeqCst)),
        Err(e) => {
            eprintln!("tautly: {e}");
            Ok(1)
        }
    }
}

#[cfg(feature = "mock")]
mod mock {
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::Context as _;
    use tautly_provider::{MockProvider, MockTurn, Provider, ProviderEvent};

    /// A provider answering with the script in `path`: a JSON array of responses, each an
    /// array of provider events (`{"text_delta": "hi"}`, `{"done": {"stop": "end_turn"}}`).
    pub fn provider(path: &Path) -> anyhow::Result<Arc<dyn Provider>> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let turns: Vec<Vec<ProviderEvent>> = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a mock script", path.display()))?;
        Ok(Arc::new(MockProvider::new(
            turns.into_iter().map(MockTurn::Events).collect(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_need_a_known_provider() {
        for ok in [
            "anthropic/claude-sonnet-5-5",
            "openai/gpt-5",
            "google/gemini-3-pro",
            "compat:local/qwen3",
        ] {
            assert_eq!(check_model(ok), Ok(()), "{ok}");
        }
        for bad in ["gpt-5", "foo/bar", "compat:/x", "anthropic/", "/x"] {
            assert!(check_model(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn flags_become_overrides_and_start_commands() {
        let cli = Cli::parse_from([
            "tautly",
            "-p",
            "hi",
            "--approve",
            "auto-edit",
            "--approve-memory",
            "--model",
            "openai/gpt-5",
            "--budget-usd",
            "1.5",
            "--no-checkpoints",
            "--approve-mcp",
        ]);
        let o = cli.overrides();
        assert_eq!(o.mode, Some(Mode::AutoEdit));
        assert_eq!(o.approve_memory, Some(true));
        assert_eq!(o.model, Some(ModelId::from("openai/gpt-5")));
        assert_eq!(o.checkpoints, Some(false));
        assert!(cli.approve_mcp);
        assert_eq!(
            cli.start_commands(),
            vec![Command::SetBudget {
                scope: BudgetScope::Session,
                max_usd: Some(1.5),
                max_tokens: None,
            }]
        );
        // Flags not given leave the configuration files in charge.
        let plain = Cli::parse_from(["tautly", "--rpc"]).overrides();
        assert_eq!(plain.mode, None);
        assert_eq!(plain.approve_memory, None);
        assert_eq!(plain.checkpoints, None);
        assert!(
            Cli::parse_from(["tautly", "--rpc"])
                .start_commands()
                .is_empty()
        );
        assert_eq!(
            Cli::parse_from(["tautly", "--approve", "yolo", "--rpc"])
                .overrides()
                .mode,
            Some(Mode::Yolo)
        );
    }
}
