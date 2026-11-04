//! `tautly`: the command-line entry point (spec 3.4).
//!
//! - `tautly -p PROMPT [--output text|json]` runs one prompt headless and exits with a
//!   code from how the turn ended (0 completed, 1 error, 2 budget / max_turns / timeout,
//!   130 cancelled).
//! - `tautly --rpc` serves the JSON-RPC protocol on stdin/stdout.
//!
//! Flags shared by both: `--approve auto-edit|yolo` (headless `ask` is otherwise denied),
//! `--approve-memory`, `--approve-mcp`, `--model ID`, `--budget-usd N` (a session budget,
//! sent before the first message), `--continue` / `--resume ID`, `--no-checkpoints`.
//! Command-line flags are trusted: they override every configuration file (spec 2.2).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{CommandFactory as _, Parser, ValueEnum};
use tautly_core::{
    BudgetScope, CliOverrides, Command, Config, Engine, EngineBuilder, Event, Mode, ModelId,
    Workspaces, install_signal_handlers,
};
use tautly_rpc::{EngineFactory, OutputFormat, Server, run_oneshot};
use tokio::io::BufReader;
use tokio::sync::mpsc;

/// How long, after the sessions have shut down, background work may hold up the exit.
const EXIT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(
    name = "tautly",
    version,
    about = "A token-efficient AI coding agent",
    after_help = "Exit codes with -p: 0 completed, 1 error, 2 budget/max turns/timeout, \
                  130 cancelled."
)]
struct Cli {
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

    /// Use a scripted provider: a JSON array of responses, each an array of provider
    /// events (tests).
    #[cfg(feature = "mock")]
    #[arg(long, value_name = "FILE", hide = true)]
    mock_script: Option<PathBuf>,
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

/// A model id must name one of the providers Tautly has an adapter for.
fn check_model(id: &str) -> Result<(), String> {
    let (provider, model) = id.split_once('/').unwrap_or((id, ""));
    let known = matches!(provider, "anthropic" | "openai" | "google")
        || provider
            .strip_prefix("compat:")
            .is_some_and(|name| !name.is_empty());
    if known && !model.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "`{id}` is not a model id: use <provider>/<model> with provider anthropic, openai, \
             google or compat:<name>"
        ))
    }
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
            #[cfg(feature = "mock")]
            provider,
        })
    }

    /// A session on `root` (this may block: configuration files, the keychain).
    fn build(
        &self,
        root: &Path,
        resume: Option<String>,
        interactive: bool,
        workspace: Option<Arc<tautly_core::Workspace>>,
    ) -> anyhow::Result<(Engine, mpsc::Receiver<Event>)> {
        let loaded = Config::load(root, self.overrides.clone())?;
        let builder = EngineBuilder {
            resume,
            interactive,
            workspace,
            approve_mcp: self.approve_mcp,
            data_home: Some(self.data_home.clone()),
            #[cfg(feature = "mock")]
            provider: self.provider.clone(),
            ..EngineBuilder::from_loaded(loaded, root)
        };
        let (engine, events) = builder.build()?;
        if let Err(e) = install_signal_handlers(&engine) {
            eprintln!("tautly: signals are not handled: {e}");
        }
        Ok((engine, events))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
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
            eprintln!("tautly: {e:#}");
            1
        }
    };
    // Every session has shut down (its event stream ended); what may be left is blocking
    // work such as a file watcher still starting.
    runtime.shutdown_timeout(EXIT_GRACE);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    if cli.prompt.is_none() && !cli.rpc {
        eprintln!(
            "tautly: the interactive REPL is not available yet; use -p PROMPT or --rpc \
             (see --help)"
        );
        return Ok(2);
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
    let (engine, events) =
        tokio::task::spawn_blocking(move || settings.build(&root, resume, false, None)).await??;
    for cmd in cli.start_commands() {
        let _ = engine.send(cmd).await;
    }
    let output = match cli.output {
        Some(Output::Json) => OutputFormat::Json,
        Some(Output::Text) | None => OutputFormat::Text,
    };
    Ok(run_oneshot(engine, events, prompt, output, tokio::io::stdout()).await)
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
    let served = Server::new(factory, workspaces, root)
        .resume(resume)
        .start_commands(cli.start_commands())
        .serve(BufReader::new(tokio::io::stdin()), tokio::io::stdout())
        .await;
    match served {
        Ok(()) => Ok(0),
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
