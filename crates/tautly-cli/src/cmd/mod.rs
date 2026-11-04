//! The utility commands (`tautly prompt|sessions|models|undo|memory|index|mcp ...`): each
//! reads or changes what a session leaves behind, in the work tree of the current
//! directory, without a model turn (`memory consolidate` aside).
//!
//! Output goes to stdout, problems to stderr; the exit code is 0, or 1 when the command
//! failed (a refused undo, an unknown session, ...). Usage errors exit 2, as clap does.

mod index;
mod mcp;
mod memory;
mod models;
mod prompt;
mod sessions;
mod undo;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use clap::Subcommand;
use tautly_core::{LogSummary, Workspace, Workspaces};

use crate::Cli;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the system prompt, or the last request a session sent.
    Prompt {
        #[command(subcommand)]
        cmd: prompt::Cmd,
    },
    /// List, show, export and delete sessions.
    Sessions {
        #[command(subcommand)]
        cmd: sessions::Cmd,
    },
    /// List the model catalog, or refresh its prices and windows.
    Models {
        #[command(subcommand)]
        cmd: models::Cmd,
    },
    /// Revert what a user turn changed (the last one by default).
    Undo(undo::Args),
    /// List and show project memory, or tidy it with the side model.
    Memory {
        #[command(subcommand)]
        cmd: memory::Cmd,
    },
    /// Report on or rebuild the code index.
    Index {
        #[command(subcommand)]
        cmd: index::Cmd,
    },
    /// List MCP servers, or import them from a Claude-style JSON file.
    Mcp {
        #[command(subcommand)]
        cmd: mcp::Cmd,
    },
}

/// Run `command`; the exit code.
pub async fn run(command: Command, cli: &Cli) -> anyhow::Result<i32> {
    let env = Env::new()?;
    match command {
        Command::Prompt { cmd } => prompt::run(cmd, &env),
        Command::Sessions { cmd } => sessions::run(cmd, &env).await,
        Command::Models { cmd } => models::run(cmd, &env).await,
        Command::Undo(args) => undo::run(args, &env).await,
        Command::Memory { cmd } => memory::run(cmd, &env, cli).await,
        Command::Index { cmd } => index::run(cmd, &env).await,
        Command::Mcp { cmd } => mcp::run(cmd, &env).await,
    }
}

/// Where a command runs: the data directory and the work tree of the current directory.
pub struct Env {
    pub data_home: PathBuf,
    pub workspace: Arc<Workspace>,
}

impl Env {
    fn new() -> anyhow::Result<Env> {
        let data_home = tautly_core::data_home().context("cannot find the data directory")?;
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        let workspace = Workspaces::new(data_home.clone()).get(&cwd)?;
        Ok(Env {
            data_home,
            workspace,
        })
    }

    pub fn root(&self) -> &Path {
        self.workspace.root()
    }

    /// The session `id` of this project: the full id, or a prefix that names one session.
    pub fn find_session(&self, id: &str) -> anyhow::Result<LogSummary> {
        let all = tautly_core::list_logs(&self.data_home, self.root())?;
        if let Some(exact) = all.iter().find(|s| s.id == id) {
            return Ok(exact.clone());
        }
        let mut named = all
            .into_iter()
            .filter(|s| !id.is_empty() && s.id.starts_with(id));
        match (named.next(), named.next()) {
            (Some(one), None) => Ok(one),
            (Some(_), Some(_)) => bail!("`{id}` names more than one session; use more of its id"),
            _ => bail!("no session `{id}` in this project (see `tautly sessions list`)"),
        }
    }

    /// The newest session that ran in this work tree.
    pub fn latest_session(&self) -> anyhow::Result<Option<LogSummary>> {
        Ok(tautly_core::list_logs_here(&self.data_home, self.root())?
            .into_iter()
            .next())
    }
}

/// `path` relative to `root`, when it is inside.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// `t` as `YYYY-MM-DD HH:MM` in UTC.
fn utc(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// `text` cut to `max` characters, with `…` when something was cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(UNIX_EPOCH), "1970-01-01 00:00");
        // 2026-11-04 12:34:56 UTC.
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(1_793_795_696)),
            "2026-11-04 12:34"
        );
        // A leap day.
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29 00:00"
        );
    }

    #[test]
    fn cut_keeps_short_text_and_marks_cuts() {
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("äöüß", 3), "äö…");
    }
}
