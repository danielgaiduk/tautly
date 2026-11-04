//! `tautly fork <session> --at N [--model M] [--prompt TEXT] [--label L] [--dir PATH]
//! [--approve auto-edit|yolo] [--budget-usd N] [--output json|text]` (spec 7.12.4):
//! branch a session at step N into a new git worktree (default
//! `<data home>/worktrees/<project-id>/<session-id>`) holding the files of that step, with
//! a session of its own (`origin: what_if`).
//!
//! - Without `--prompt` it prints `worktree: <path>  session: <id>  resume: cd <path> &&
//!   tautly --resume <id>` (with `--output json`, one object with the same).
//! - With `--prompt` it prints the worktree line on stderr and runs the prompt in the
//!   worktree as `tautly -p` does: headless (what would ask is denied unless `--approve`),
//!   with the same output and exit codes. The fork decides every call by its own policy:
//!   nothing the parent's user approved carries over.
//!
//! The original work tree and its index are never touched. Outside git it is refused.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use clap::Args as ClapArgs;
use serde_json::json;
use tautly_core::worktree::{ForkRequest, fork_to_worktree};
use tautly_core::{BudgetScope, Command, Mode, ModelId, printable};

use super::Env;
use crate::{Approve, Cli, Output, Settings};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The session id, or a prefix of it.
    session: String,
    /// The step to fork at (as `tautly sessions show` numbers them; 0: before the first).
    #[arg(long, value_name = "N")]
    at: u32,
    /// The model the fork continues on, as <provider>/<model>.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
    /// Run TEXT in the fork's worktree as one headless turn, as `tautly -p` does.
    #[arg(long, value_name = "TEXT")]
    prompt: Option<String>,
    /// The fork's name.
    #[arg(long, value_name = "L")]
    label: Option<String>,
    /// Where the worktree goes (default: <data home>/worktrees/<project-id>/<session-id>).
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
    /// With --prompt: allow edits (auto-edit) or everything no rule stops (yolo) without
    /// asking; otherwise what would ask is denied.
    #[arg(long, value_enum, value_name = "MODE", requires = "prompt")]
    approve: Option<Approve>,
    /// With --prompt: stop once the fork has cost this many US dollars.
    #[arg(long, value_name = "USD", value_parser = crate::parse_usd, requires = "prompt")]
    budget_usd: Option<f64>,
    /// What it prints: text (default), or JSON (one object; with --prompt, every event).
    #[arg(long, value_enum, value_name = "FORMAT")]
    output: Option<Output>,
}

pub async fn run(args: Args, env: &Env, cli: &Cli) -> anyhow::Result<i32> {
    if let Some(model) = &args.model {
        crate::check_model(model).map_err(anyhow::Error::msg)?;
    }
    let session = env.find_session(&args.session)?;
    let dir = match args.dir {
        Some(d) if d.is_relative() => Some(
            std::env::current_dir()
                .context("cannot read the current directory")?
                .join(d),
        ),
        other => other,
    };
    let request = ForkRequest {
        at: Some(args.at),
        dir,
        label: args.label,
        model: args.model.as_deref().map(ModelId::from),
    };
    let made = fork_to_worktree(
        Arc::clone(env.workspace.sandbox()),
        &env.data_home,
        &session.path,
        request,
    )
    .await?;
    if let Some(note) = &made.note {
        eprintln!("tautly: note: {}", printable(note));
    }
    let path = printable(&made.worktree.display().to_string());
    let id = printable(&made.session_id);
    let resume = format!(
        "cd {} && tautly --resume {id}",
        printable(&shell_quoted(&made.worktree))
    );
    let Some(prompt) = args.prompt else {
        match args.output {
            Some(Output::Json) => println!(
                "{}",
                json!({
                    "worktree": made.worktree.to_string_lossy(),
                    "session_id": made.session_id,
                    "at": made.at,
                    "resume": resume,
                    "note": made.note,
                })
            ),
            Some(Output::Text) | None => {
                println!("worktree: {path}  session: {id}  resume: {resume}");
            }
        }
        return Ok(0);
    };
    eprintln!("worktree: {path}  session: {id}");
    let mut settings = Settings::new(cli)?;
    if let Some(approve) = args.approve {
        settings.overrides.mode = Some(match approve {
            Approve::AutoEdit => Mode::AutoEdit,
            Approve::Yolo => Mode::Yolo,
        });
    }
    if let Some(model) = &args.model {
        // So no provider is detected for a model the fork does not use.
        settings.overrides.model = Some(ModelId::from(model.as_str()));
    }
    let start = args
        .budget_usd
        .map(|usd| Command::SetBudget {
            scope: BudgetScope::Session,
            max_usd: Some(usd),
            max_tokens: None,
        })
        .into_iter()
        .collect();
    crate::headless(
        settings,
        made.worktree,
        Some(made.session_id),
        prompt,
        start,
        crate::output_format(args.output),
    )
    .await
}

/// `path` as a shell word: as it is when it has only safe characters, else in single
/// quotes.
fn shell_quoted(path: &Path) -> String {
    let text = path.display().to_string();
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_quoted_for_the_shell_only_when_needed() {
        assert_eq!(shell_quoted(Path::new("/a/b-c_d.e")), "/a/b-c_d.e");
        assert_eq!(shell_quoted(Path::new("/a b/c")), "'/a b/c'");
        assert_eq!(shell_quoted(Path::new("/it's")), r"'/it'\''s'");
    }
}
