//! `tautly memory list|show|consolidate` (spec 7.4).

use std::io::BufRead as _;
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::bail;
use clap::Subcommand;
use tautly_core::{Command as EngineCommand, Event, MemoryChange, MemoryEntry};

use super::{Env, cut};
use crate::{Cli, Settings};

/// Longest a command waits for the code index (whose hashes decide what is stale).
const READY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// The project's memory entries; stale ones are marked.
    List,
    /// One entry: its text, anchors and whether its code changed.
    Show {
        /// The entry id (see `memory list`).
        id: String,
    },
    /// Ask the side model to merge or drop entries; you approve each change.
    Consolidate,
}

pub async fn run(cmd: Cmd, env: &Env, cli: &Cli) -> anyhow::Result<i32> {
    match cmd {
        Cmd::List => {
            let entries = entries(env).await?;
            if entries.is_empty() {
                println!("no memory entries in this project");
                return Ok(0);
            }
            let width = entries.iter().map(|e| e.id.len()).max().unwrap_or(2).max(2);
            for e in &entries {
                println!(
                    "{:<width$}  {:<10}  {}{}",
                    e.id,
                    e.kind.as_str(),
                    e.title,
                    if e.stale { "  [stale]" } else { "" }
                );
            }
            Ok(0)
        }
        Cmd::Show { id } => {
            let entries = entries(env).await?;
            let Some(entry) = entries.iter().find(|e| e.id == id) else {
                bail!("no memory entry `{id}` (see `tautly memory list`)");
            };
            print!("{}", entry.to_markdown());
            println!(
                "\nstatus: {}",
                if entry.stale {
                    "stale (code it is anchored to changed since it was written)"
                } else {
                    "current"
                }
            );
            Ok(0)
        }
        Cmd::Consolidate => consolidate(env, cli).await,
    }
}

/// The project's entries, with staleness checked against the code index as it is now.
async fn entries(env: &Env) -> anyhow::Result<Vec<MemoryEntry>> {
    let ws = &env.workspace;
    ws.start();
    let ready = ws.wait_ready(READY_TIMEOUT).await;
    let Some(memory) = ws.memory() else {
        bail!("project memory is not available (the code index could not be opened)");
    };
    if !ready {
        eprintln!("tautly: the code index is still being built; staleness may be incomplete");
    }
    tokio::task::spawn_blocking(move || {
        let mut store = memory.write().unwrap_or_else(PoisonError::into_inner);
        store.refresh_staleness();
        if store.skipped_count() > 0 {
            eprintln!(
                "tautly: {} memory file{} could not be loaded",
                store.skipped_count(),
                if store.skipped_count() == 1 { "" } else { "s" }
            );
        }
        store.entries().to_vec()
    })
    .await
    .map_err(|e| anyhow::anyhow!("reading memory failed: {e}"))
}

/// `consolidate`: a session with the side model proposes merges and deletions
/// (`ConsolidateMemory`); each one is shown and applied only when you answer `y`.
async fn consolidate(env: &Env, cli: &Cli) -> anyhow::Result<i32> {
    let mut settings = Settings::new(cli)?;
    // Applied only on approval, whatever the configuration says about proposals.
    settings.overrides.approve_memory = Some(false);
    let root = env.root().to_path_buf();
    let (engine, mut events) =
        tokio::task::spawn_blocking(move || settings.build(&root, None, true, None)).await??;
    let session_id = engine.current_session().0;
    let _ = engine.send(EngineCommand::ConsolidateMemory).await;
    // The engine runs commands in order and answers `Diff` at once: its answer says the
    // consolidation is over, which has no event of its own.
    let _ = engine.send(EngineCommand::Diff { turn: None }).await;

    let mut proposals = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            Event::MemoryProposed {
                proposal_id,
                change,
                replaces,
            } => proposals.push((proposal_id, change, replaces)),
            Event::Warning { message } => eprintln!("tautly: {message}"),
            Event::SessionDiff { .. } => break,
            // Consolidation reports its own problems as warnings, so an error is the
            // answer to the barrier (`diff: ...`), or the session is lost (it would never
            // answer): either way nothing more comes.
            Event::Error { message, hint, .. } => {
                if !message.starts_with("diff:") {
                    eprintln!("tautly: {message}");
                    if let Some(hint) = hint {
                        eprintln!("tautly: {hint}");
                    }
                }
                break;
            }
            _ => {}
        }
    }

    let (mut approved, mut rejected) = (Vec::new(), 0);
    let mut input = std::io::stdin().lock();
    let mut asking = true;
    let total = proposals.len();
    for (n, (proposal_id, change, replaces)) in proposals.into_iter().enumerate() {
        show(n + 1, total, &change, &replaces);
        let yes = asking && {
            print!("Apply this change? [y/N] ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            // Nothing more to read: the rest are refused.
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    asking = false;
                    println!();
                    false
                }
                Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
            }
        };
        if yes {
            approved.push(proposal_id.clone());
            let _ = engine
                .send(EngineCommand::ApproveMemory { proposal_id })
                .await;
        } else {
            rejected += 1;
            let _ = engine
                .send(EngineCommand::RejectMemory { proposal_id })
                .await;
        }
    }
    let _ = engine.send(EngineCommand::Shutdown).await;
    let mut warnings = Vec::new();
    while let Some(event) = events.recv().await {
        if let Event::Warning { message } = event {
            eprintln!("tautly: {message}");
            warnings.push(message);
        }
    }
    drop(engine);
    // An approval the engine could not save stays pending (and says so): not applied.
    let failed = approved
        .iter()
        .filter(|id| {
            warnings.iter().any(|w| {
                w.contains(id.as_str())
                    && (w.contains("could not be saved") || w.contains("is pending"))
            })
        })
        .count();
    let applied = approved.len() - failed;
    if total == 0 {
        println!("the side model proposed no changes");
    } else if failed > 0 {
        println!("{applied} applied, {rejected} rejected, {failed} could not be saved");
    } else {
        println!("{applied} applied, {rejected} rejected");
    }
    // The memory files are the result; the session that made the proposals has no turns
    // and must not become the newest session (`undo`, `--continue`).
    if let Err(e) = remove_session(env, &session_id).await {
        eprintln!("tautly: could not remove the consolidation session {session_id}: {e}");
    }
    Ok(0)
}

/// Delete session `id` once the engine has let go of its log (it does so as it stops).
async fn remove_session(env: &Env, id: &str) -> anyhow::Result<()> {
    let sandbox = env.workspace.sandbox();
    let mut last = None;
    for _ in 0..40 {
        match tautly_core::sessions::delete(&env.data_home, sandbox.as_ref(), id).await {
            Ok(_) => return Ok(()),
            Err(e @ tautly_core::sessions::DeleteError::Running(_)) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last.map_or_else(|| anyhow::anyhow!("still running"), Into::into))
}

/// One proposal: what it writes and which entries it replaces or deletes.
fn show(n: usize, total: usize, change: &MemoryChange, replaces: &[MemoryEntry]) {
    println!("\nproposal {n} of {total}");
    for id in &change.delete {
        let title = replaces
            .iter()
            .find(|e| e.id == *id)
            .map(|e| format!("  \"{}\"", e.title))
            .unwrap_or_default();
        println!("  - delete {id}{title}");
        if let Some(old) = replaces.iter().find(|e| e.id == *id) {
            println!("      {}", cut(old.body.lines().next().unwrap_or(""), 100));
        }
    }
    if let Some(entry) = &change.upsert {
        println!(
            "  + write {} [{}]  \"{}\"",
            entry.id,
            entry.kind.as_str(),
            entry.title
        );
        for line in entry.body.lines() {
            println!("      {line}");
        }
    }
}
