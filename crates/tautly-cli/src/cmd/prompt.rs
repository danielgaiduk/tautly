//! `tautly prompt show [--full]` (spec 6.1): the system prompt, or the exact last request.

use anyhow::{Context as _, bail};
use clap::Subcommand;

use super::Env;

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Print the system prompt; with --full, the exact last request as JSON.
    Show {
        /// The last request of the newest session of this work tree, as JSON.
        #[arg(long)]
        full: bool,
    },
}

pub fn run(cmd: Cmd, env: &Env) -> anyhow::Result<i32> {
    let Cmd::Show { full } = cmd;
    if !full {
        let prompt = tautly_core::system_prompt();
        print!("{prompt}");
        if !prompt.ends_with('\n') {
            println!();
        }
        return Ok(0);
    }
    // The newest session that has sent a request (a session opened and closed at once
    // has none, and must not hide the one before it).
    for session in tautly_core::list_logs_here(&env.data_home, env.root())? {
        let file = session.path.with_extension("last-request.json");
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
        };
        let request: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not JSON", file.display()))?;
        println!("{}", serde_json::to_string_pretty(&request)?);
        return Ok(0);
    }
    bail!("no request recorded: no session of this work tree has sent one yet")
}
