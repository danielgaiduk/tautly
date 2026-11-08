//! `tautly index status|rebuild` (spec 7.1): what the code index of this work tree holds.

use std::time::Duration;

use anyhow::bail;
use clap::Subcommand;

use super::Env;

/// Longest a command waits for the index to finish its first scan of the work tree.
const READY_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Files, symbols and references the code index holds for this work tree.
    Status,
    /// Delete the code index of this work tree and build it again.
    Rebuild,
}

pub async fn run(cmd: Cmd, env: &Env) -> anyhow::Result<i32> {
    let ws = &env.workspace;
    if matches!(cmd, Cmd::Rebuild) {
        // A running session holds the index's lock shared: with one running, the database
        // is not deleted from under it. Held through the deletes, released before the
        // rebuild below opens the index again.
        let lock = ws.try_lock_index_exclusive()?;
        let db = ws.index_db();
        for suffix in ["", "-wal", "-shm"] {
            let mut file = db.clone().into_os_string();
            file.push(suffix);
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!(
                    "cannot remove {}: {e}",
                    std::path::Path::new(&file).display()
                ),
            }
        }
        drop(lock);
    }
    // Opening brings the index up to date with the files: the counts are the work tree's.
    ws.start();
    let ready = ws.wait_ready(READY_TIMEOUT).await;
    let Some(index) = ws.index() else {
        bail!(
            "the code index could not be opened (database {})",
            ws.index_db().display()
        );
    };
    let stats = index.stats();
    println!("database: {}", ws.index_db().display());
    println!("files: {}", stats.files);
    println!("symbols: {}", stats.symbols);
    println!("references: {}", stats.refs);
    if let Some(e) = index.last_error() {
        println!("last error: {}", tautly_core::printable(&e.to_string()));
    }
    if !ready {
        println!(
            "the index was still being built after {} s; the counts are partial",
            READY_TIMEOUT.as_secs()
        );
    }
    Ok(0)
}
