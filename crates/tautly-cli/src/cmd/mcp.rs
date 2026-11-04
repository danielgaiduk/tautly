//! `tautly mcp list|import` (spec 7.11.10).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use clap::Subcommand;
use tautly_core::{
    Approval, CliOverrides, Config, McpHub, ServerStatus, import_into_toml, parse_mcp_json,
    plan_servers, provider_key_vars,
};

use super::Env;

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// The MCP servers of this work tree: source, approval, state and tool count.
    List,
    /// Add the servers of a Claude-style JSON file (`mcpServers`) to the user config.
    Import {
        /// The file (default: ~/.claude.json, else ./.mcp.json).
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
    },
}

pub async fn run(cmd: Cmd, env: &Env) -> anyhow::Result<i32> {
    match cmd {
        Cmd::List => list(env).await,
        Cmd::Import { from } => import(from),
    }
}

/// `list`: the servers the next session here would know, with the state they reach. Servers
/// that would start (the user's, and repository ones already approved) are started for a
/// moment to see their tools; repository servers nobody approved are never started.
async fn list(env: &Env) -> anyhow::Result<i32> {
    let loaded = Config::load(env.root(), CliOverrides::default())?;
    for warning in &loaded.warnings {
        eprintln!("tautly: {warning}");
    }
    let config = loaded.config;
    let sandbox = env.workspace.sandbox();
    let plan = plan_servers(
        &config.mcp,
        &config.project_mcp_servers,
        &[],
        false,
        &env.data_home,
        sandbox.as_ref(),
    )
    .await;
    for warning in &plan.warnings {
        eprintln!("tautly: {warning}");
    }
    if plan.servers.is_empty() {
        println!(
            "{}",
            if config.mcp.enabled {
                "no MCP servers configured"
            } else {
                "no MCP servers: MCP is turned off ([mcp] enabled = false)"
            }
        );
        return Ok(0);
    }
    let start = plan
        .servers
        .iter()
        .filter(|s| matches!(s.approval, Approval::Trusted | Approval::Remembered))
        .map(|s| (s.name.clone(), s.spec.clone(), s.source))
        .collect();
    let hub = McpHub::start(
        start,
        Arc::clone(sandbox),
        &config.mcp,
        &provider_key_vars(&config),
    )
    .await;
    hub.wait_started(Duration::from_millis(config.mcp.startup_timeout_ms))
        .await;
    let status: HashMap<String, ServerStatus> = hub
        .status()
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    hub.shutdown().await;

    let width = plan
        .servers
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<width$}  {:<8}  {:<9}  {:<12}  TOOLS",
        "NAME", "SOURCE", "APPROVAL", "STATE"
    );
    let mut pending = false;
    for s in &plan.servers {
        let approval = match s.approval {
            Approval::Trusted => "trusted",
            Approval::Remembered => "approved",
            Approval::ThisRun => "this run",
            Approval::Pending => {
                pending = true;
                "pending"
            }
        };
        let (state, tools, message) = match status.get(&s.name) {
            Some(st) => (
                serde_json::to_value(st.state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
                    .replace('_', " "),
                st.tools.to_string(),
                st.message.clone(),
            ),
            None => ("not started".to_string(), "-".to_string(), None),
        };
        let message = message.map(|m| format!("  ({m})")).unwrap_or_default();
        println!(
            "{:<width$}  {:<8}  {approval:<9}  {state:<12}  {tools}{message}",
            s.name,
            s.source.as_str()
        );
    }
    if pending {
        println!(
            "\nServers the repository declares need your approval before they start: approve \
             them in an interactive session, or run with --approve-mcp to start them once."
        );
    }
    Ok(0)
}

/// `import`: servers of a Claude-style JSON file into the user's `config.toml`, whose
/// comments and layout are kept. A server the config already has is left alone.
fn import(from: Option<PathBuf>) -> anyhow::Result<i32> {
    let (path, text) = read_source(from)?;
    let (servers, skipped) =
        parse_mcp_json(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    for line in &skipped {
        println!("{line}");
    }
    if servers.is_empty() {
        println!("no MCP servers to import in {}", path.display());
        return Ok(0);
    }
    let config = tautly_core::config_home()
        .map_err(|e| anyhow!("no config directory: {e}"))?
        .join("config.toml");
    // A config file that is a symlink (a dotfile manager) is written through.
    let target = std::fs::canonicalize(&config).unwrap_or(config);
    let existing = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", target.display())),
    };
    let (updated, kept) =
        import_into_toml(&existing, &servers).map_err(|e| anyhow!("{}: {e}", target.display()))?;
    let added: Vec<&str> = servers
        .keys()
        .map(String::as_str)
        .filter(|n| !kept.iter().any(|k| k == n))
        .collect();
    if !added.is_empty() {
        write(&target, &updated)?;
        println!(
            "imported {} server{} into {}: {}",
            added.len(),
            if added.len() == 1 { "" } else { "s" },
            target.display(),
            added.join(", ")
        );
    }
    if !kept.is_empty() {
        println!("already in the config, left alone: {}", kept.join(", "));
    }
    Ok(0)
}

/// The file to import from: `--from`, else `~/.claude.json`, else `./.mcp.json`.
fn read_source(from: Option<PathBuf>) -> anyhow::Result<(PathBuf, String)> {
    let read = |p: &Path| {
        std::fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))
    };
    if let Some(p) = from {
        let text = read(&p)?;
        return Ok((p, text));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".claude.json"));
    for candidate in home.into_iter().chain([PathBuf::from(".mcp.json")]) {
        if candidate.is_file() {
            let text = read(&candidate)?;
            return Ok((candidate, text));
        }
    }
    bail!("found neither ~/.claude.json nor ./.mcp.json; name the file with --from PATH")
}

/// Replace `file` with `text` through a temporary file next to it, keeping its mode.
fn write(file: &Path, text: &str) -> anyhow::Result<()> {
    let dir = file.parent().context("the config file has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(file) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("cannot write {}: {e}", file.display())
    })
}
