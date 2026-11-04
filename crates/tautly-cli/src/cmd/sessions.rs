//! `tautly sessions list|show|tree|export|delete` (spec 6.5, 7.12.3).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;

use anyhow::{Context as _, bail};
use clap::{Subcommand, ValueEnum};
use tautly_core::{LogSummary, SessionReader, SessionRecord};

use super::{Env, cut, utc};

/// Longest tool result or argument text a transcript or listing shows.
const RESULT_CHARS: usize = 400;
const ARGS_CHARS: usize = 160;

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// The sessions of this work tree, newest first.
    List,
    /// One session: its header and its user turns.
    Show {
        /// The session id, or a prefix of it.
        id: String,
    },
    /// The project's sessions as a tree of branches.
    Tree,
    /// Print a session as its raw log (jsonl) or as a readable transcript (md).
    Export {
        /// The session id, or a prefix of it.
        id: String,
        #[arg(long, value_enum, default_value = "md")]
        format: Format,
    },
    /// Delete a session's log and its git checkpoint refs.
    Delete {
        /// The full session id.
        id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Jsonl,
    Md,
}

pub async fn run(cmd: Cmd, env: &Env) -> anyhow::Result<i32> {
    match cmd {
        Cmd::List => list(env),
        Cmd::Show { id } => show(env, &id),
        Cmd::Tree => tree(env),
        Cmd::Export { id, format } => export(env, &id, format),
        Cmd::Delete { id } => delete(env, &id).await,
    }
}

fn list(env: &Env) -> anyhow::Result<i32> {
    let all = tautly_core::sessions::list_in(&env.data_home, env.root(), usize::MAX);
    if all.is_empty() {
        println!("no sessions in this work tree");
        return Ok(0);
    }
    println!(
        "{:<26}  {:<16}  {:>8}  FIRST PROMPT",
        "ID", "STARTED (UTC)", "COST"
    );
    for s in all {
        let cost = s
            .cost_usd
            .map_or_else(|| "-".to_string(), |c| format!("${c:.4}"));
        println!(
            "{:<26}  {:<16}  {cost:>8}  {}",
            s.id,
            utc(s.started),
            s.first_prompt
        );
    }
    Ok(0)
}

/// One indented line per branch, as `/tree` prints it (no session is current here).
fn tree(env: &Env) -> anyhow::Result<i32> {
    let nodes = tautly_core::tree::for_root(&env.data_home, env.root(), None)
        .context("cannot list the sessions")?;
    if nodes.is_empty() {
        println!("no sessions in this project");
        return Ok(0);
    }
    for line in tautly_core::tree::lines(&nodes) {
        println!("{line}");
    }
    Ok(0)
}

fn open(session: &LogSummary) -> anyhow::Result<SessionReader<SessionRecord>> {
    SessionReader::<SessionRecord>::open(&session.path)
        .with_context(|| format!("cannot read session {}", session.id))
}

/// One user turn of a log.
struct Turn {
    n: u32,
    text: String,
    first_step: Option<u32>,
    last_step: Option<u32>,
    reason: Option<String>,
    files: BTreeSet<String>,
    undone: bool,
}

fn turns(reader: &SessionReader<SessionRecord>) -> anyhow::Result<Vec<Turn>> {
    let mut turns: Vec<Turn> = Vec::new();
    for rec in reader.records() {
        let (_, rec) = rec?;
        match rec {
            SessionRecord::TurnStart { turn, text } => turns.push(Turn {
                n: turn,
                text,
                first_step: None,
                last_step: None,
                reason: None,
                files: BTreeSet::new(),
                undone: false,
            }),
            SessionRecord::Request { step, .. } => {
                if let Some(t) = turns.last_mut() {
                    t.first_step.get_or_insert(step);
                    t.last_step = Some(step);
                }
            }
            SessionRecord::Edit {
                path,
                asserted: false,
                ..
            } => {
                if let Some(t) = turns.last_mut() {
                    t.files.insert(path.as_str().to_string());
                }
            }
            SessionRecord::TurnEnd { reason, .. } => {
                if let Some(t) = turns.last_mut() {
                    t.reason = serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string));
                }
            }
            SessionRecord::Undone { turn, .. } => {
                for t in turns.iter_mut().filter(|t| t.n == turn) {
                    t.undone = true;
                }
            }
            _ => {}
        }
    }
    Ok(turns)
}

fn show(env: &Env, id: &str) -> anyhow::Result<i32> {
    let session = env.find_session(id)?;
    let reader = open(&session)?;
    let header = reader.header();
    println!("session  {}", header.session_id);
    println!("started  {} UTC", utc(header.created));
    println!("root     {}", header.root.display());
    if let Some(model) = &header.model {
        println!("model    {model}");
    }
    if let Some(label) = &session.label {
        println!("label    {label}");
    }
    if let Some((parent, offset)) = &header.parent {
        println!("branch   of {parent}, from record {offset}");
    }
    let ledger_file = env.data_home.join("ledger.sqlite");
    if let Some(t) = ledger_file
        .exists()
        .then(|| tautly_core::Ledger::open(&ledger_file).ok())
        .flatten()
        .and_then(|l| l.session_totals(&header.session_id).ok())
        .filter(|t| t.requests > 0)
    {
        let unpriced = if t.unpriced > 0 {
            format!(", {} unpriced", t.unpriced)
        } else {
            String::new()
        };
        println!("cost     ${:.4} ({} requests{unpriced})", t.usd, t.requests);
    }
    let turns = turns(&reader)?;
    if turns.is_empty() {
        println!("no user turns");
        return Ok(0);
    }
    println!("turns");
    for t in &turns {
        let steps = match (t.first_step, t.last_step) {
            (Some(a), Some(b)) if a != b => format!("steps {a}-{b}"),
            (Some(a), _) => format!("step {a}"),
            _ => "no requests".to_string(),
        };
        let mut notes = vec![t.reason.clone().unwrap_or_else(|| "unfinished".into())];
        if !t.files.is_empty() {
            let n = t.files.len();
            notes.push(format!("{n} file{} edited", if n == 1 { "" } else { "s" }));
        }
        if t.undone {
            notes.push("undone".into());
        }
        let prompt = t.text.lines().next().unwrap_or("");
        println!(
            "  {}  {steps}  {}  {}",
            t.n,
            cut(prompt, 60),
            notes.join(", ")
        );
    }
    Ok(0)
}

fn export(env: &Env, id: &str, format: Format) -> anyhow::Result<i32> {
    let session = env.find_session(id)?;
    match format {
        Format::Jsonl => {
            let bytes = std::fs::read(&session.path)
                .with_context(|| format!("cannot read {}", session.path.display()))?;
            std::io::stdout().write_all(&bytes)?;
        }
        Format::Md => print!("{}", markdown(&open(&session)?)?),
    }
    Ok(0)
}

/// The session as a transcript: each user turn, what the model said, the tool calls with
/// their (cut) results, the files edited.
fn markdown(reader: &SessionReader<SessionRecord>) -> anyhow::Result<String> {
    let header = reader.header();
    let mut md = format!("# Session {}\n\n", header.session_id);
    let _ = writeln!(md, "- started: {} UTC", utc(header.created));
    let _ = writeln!(md, "- root: `{}`", header.root.display());
    if let Some(model) = &header.model {
        let _ = writeln!(md, "- model: `{model}`");
    }
    let mut said = String::new();
    let flush = |md: &mut String, said: &mut String| {
        if !said.trim().is_empty() {
            let _ = write!(md, "\n{}\n", said.trim());
        }
        said.clear();
    };
    for rec in reader.records() {
        let (_, rec) = rec?;
        match rec {
            SessionRecord::TurnStart { turn, text } => {
                flush(&mut md, &mut said);
                let _ = write!(md, "\n## Turn {turn}\n\n**You:** {}\n", text.trim());
            }
            SessionRecord::Provider(tautly_core::ProviderEvent::TextDelta(t)) => said.push_str(&t),
            SessionRecord::ToolCall { name, args, .. } => {
                flush(&mut md, &mut said);
                let _ = write!(
                    md,
                    "\n- tool `{name}` {}\n",
                    cut(&args.to_string(), ARGS_CHARS)
                );
            }
            SessionRecord::ToolResult {
                result_blob,
                is_error,
                ..
            } => {
                let result = reader
                    .read_blob(&result_blob)
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_else(|e| format!("(result not available: {e})"));
                let label = if is_error { "error" } else { "result" };
                let _ = writeln!(md, "  {label}:");
                for line in cut(result.trim(), RESULT_CHARS).lines() {
                    let _ = writeln!(md, "      {line}");
                }
            }
            SessionRecord::Edit {
                path,
                asserted: false,
                ..
            } => {
                let _ = writeln!(md, "  edited `{}`", path.as_str());
            }
            SessionRecord::Undone { turn, .. } => {
                flush(&mut md, &mut said);
                let _ = write!(md, "\n_Turn {turn} was undone._\n");
            }
            SessionRecord::TurnEnd { reason, .. } => {
                flush(&mut md, &mut said);
                let reason = serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                let _ = write!(md, "\n_Turn ended: {reason}._\n");
            }
            _ => {}
        }
    }
    flush(&mut md, &mut said);
    Ok(md)
}

async fn delete(env: &Env, id: &str) -> anyhow::Result<i32> {
    if id.is_empty() {
        bail!("give the session's id");
    }
    let done =
        tautly_core::sessions::delete(&env.data_home, env.workspace.sandbox().as_ref(), id).await?;
    println!(
        "deleted session {id} ({}); removed {} checkpoint ref{}",
        done.log.display(),
        done.refs,
        if done.refs == 1 { "" } else { "s" }
    );
    Ok(0)
}
