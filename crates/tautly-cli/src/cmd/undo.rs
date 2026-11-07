//! `tautly undo [--turn N] [--dry-run]` (spec 7.11.11): the offline form of `Command::Undo`,
//! on a session no engine holds open. It shares `tautly_core::undo::undo` with the engine:
//! the same user-turn numbering, the same refusals, the same git checkpoints.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
use clap::Args as ClapArgs;
use tautly_core::checkpoint::EXCLUDES_DIR;
use tautly_core::undo::{UndoError, UndoReport};
use tautly_core::{CheckpointConfig, Checkpointer, CliOverrides, Config, LogError, SessionReader};
use tautly_core::{LogSummary, SessionRecord};

use super::{Env, rel};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The user turn to undo (default: the last one that started).
    #[arg(long, value_name = "N")]
    pub turn: Option<u32>,
    /// Show what would change; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// The session (default: the newest of this work tree); the id or a prefix.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
}

pub async fn run(args: Args, env: &Env) -> anyhow::Result<i32> {
    let session = target(&args, env)?;
    let reader = SessionReader::<SessionRecord>::open(&session.path)
        .with_context(|| format!("cannot read session {}", session.id))?;
    // The checkpoint settings (timeouts, limits) are the user's; whether new checkpoints
    // are still taken does not matter to reading old ones.
    let config = Config::load(env.root(), CliOverrides::default())?;
    for warning in &config.warnings {
        eprintln!("tautly: {}", tautly_core::printable_multiline(warning));
    }
    let mut checkpoints = checkpointer(env, &session, config.config.checkpoints);
    // The working-tree lease, exclusive across the check and the write: no live session's
    // tool may write in between (CP5 review M3). A dry run writes nothing.
    let busy =
        || anyhow!("a running tautly uses this work tree; use /undo there, or stop it first");
    let lease = match args.dry_run {
        true => None,
        false => {
            Some(tautly_core::Lease::try_shared(&env.data_home, env.root())?.ok_or_else(busy)?)
        }
    };
    let _alone = match lease.as_ref().map(tautly_core::Lease::try_exclusive) {
        Some(Err(tautly_core::LeaseError::Busy)) => return Err(busy()),
        other => other.transpose()?,
    };
    let done = tautly_core::undo::undo(
        &reader,
        env.workspace.sandbox().as_ref(),
        checkpoints.as_mut(),
        args.turn,
        args.dry_run,
    )
    .await;
    match done {
        Ok(report) => {
            print_report(&report, &session, env.root());
            Ok(0)
        }
        Err(e) => refusal(e, &args, &session),
    }
}

/// The session to undo in: the one named, or the newest of this work tree. A session of
/// another work tree is refused: its files are not here.
fn target(args: &Args, env: &Env) -> anyhow::Result<LogSummary> {
    match &args.session {
        Some(id) => {
            let session = env.find_session(id)?;
            if session.root != env.root() {
                bail!(
                    "session {} ran in {}; run `tautly undo` there",
                    session.id,
                    session.root.display()
                );
            }
            Ok(session)
        }
        None => env
            .latest_session()?
            .ok_or_else(|| anyhow!("nothing to undo: no session in this work tree")),
    }
}

/// The work tree's checkpointer (none outside git, or without a `git` on `PATH`).
fn checkpointer(env: &Env, session: &LogSummary, cfg: CheckpointConfig) -> Option<Checkpointer> {
    let cp = Checkpointer::new(
        Arc::clone(env.workspace.sandbox()),
        env.root().to_path_buf(),
        session.id.clone(),
        cfg,
    )?;
    // Each checkpoint's ignore rules are kept next to the project's session logs.
    Some(match session.path.parent() {
        Some(dir) => cp.with_excludes_store(dir.join(EXCLUDES_DIR)),
        None => cp,
    })
}

fn files(n: usize) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

fn print_report(report: &UndoReport, session: &LogSummary, root: &Path) {
    let (restored, deleted) = (report.restored.len(), report.deleted.len());
    if report.dry_run {
        println!(
            "dry run: undoing turn {} of session {} would restore {} and delete {}",
            report.turn,
            session.id,
            files(restored),
            files(deleted)
        );
    } else {
        println!(
            "undid turn {} of session {}: restored {}, deleted {}",
            report.turn,
            session.id,
            files(restored),
            files(deleted)
        );
    }
    let (restore, delete) = if report.dry_run {
        ("restore", "delete")
    } else {
        ("restored", "deleted")
    };
    for path in &report.restored {
        println!("  {restore} {}", rel(root, path));
    }
    for path in &report.deleted {
        println!("  {delete} {}", rel(root, path));
    }
    if let Some(backup) = &report.backup_ref {
        println!(
            "backup of the work tree before the undo: {backup} (a git ref; see `git show {backup}`)"
        );
    }
    if let Some(note) = &report.note {
        println!("note: {note}");
    }
    if report.dry_run {
        println!("nothing was changed");
    }
}

/// An undo that did not happen: the refusals say which files and what to do (exit 1); the
/// rest are errors.
fn refusal(e: UndoError, args: &Args, session: &LogSummary) -> anyhow::Result<i32> {
    let which = args
        .turn
        .map_or_else(|| "the last turn".to_string(), |t| format!("turn {t}"));
    let root = session.root.as_path();
    let list = |paths: &[std::path::PathBuf]| -> String {
        paths
            .iter()
            .map(|p| format!("  {}\n", rel(root, p)))
            .collect()
    };
    match e {
        UndoError::Changed { paths } => {
            eprintln!(
                "tautly: cannot undo {which}: these files changed since the turn:\n{}\
                 nothing was written. Check them (`git diff`), put them back or keep them, \
                 then undo again.",
                list(&paths)
            );
            Ok(1)
        }
        UndoError::LaterTurns { turns, paths } => {
            let newest = turns.iter().max().copied().unwrap_or(0);
            let mut listed: Vec<String> = turns.iter().map(u32::to_string).collect();
            listed.dedup();
            let again = match &args.session {
                Some(id) => format!("tautly undo --turn {newest} --session {id}"),
                None => format!("tautly undo --turn {newest}"),
            };
            eprintln!(
                "tautly: cannot undo {which}: later turn{} ({}) changed these files:\n{}\
                 nothing was written. Undo those turns first, newest first: {again}",
                if turns.len() == 1 { "" } else { "s" },
                listed.join(", "),
                list(&paths)
            );
            Ok(1)
        }
        UndoError::Session(LogError::AlreadyOpen(_)) => bail!(
            "session {} is open in a running tautly; use /undo there, or stop it first",
            session.id
        ),
        UndoError::NoSuchTurn { turns: 0, .. } => {
            bail!("nothing to undo: session {} has no user turns", session.id)
        }
        other => Err(other.into()),
    }
}
