//! `tautly compare <a> <b> [<c>…] [--json]` (spec 7.12.6): two to five sessions of this
//! project side by side, from their logs (and the cost ledger) alone; no model or tool
//! runs. When the sessions share a fork point the figures count from it, otherwise each
//! covers its own records (a branch's from its fork record). Every cost is the logged
//! usage at the current catalog; the ledger's figure (own records only) is its own line.
//!
//! The text form prints one column per session (its label, or the shortest prefix of its
//! id that tells the sessions apart): models, user turns and steps, tokens, cost, wall
//! time, tool calls, blocked calls, edit failures, the last test run (never its command,
//! which may name paths), files touched and the net diff stat (tool edits only: what
//! commands changed is not in the logs, and a session's notes count the commands that may
//! have written files); then the fork point, a files table (`only <x>`, `both`/`all`, or
//! the sessions that touched it) and the notes. Two sessions with one label are told apart
//! by an id prefix. Everything a log supplies is escaped. `--json` prints one object:
//! `fork_point` (`{session_id, step, offset}` or null), `sessions` (each session's
//! figures, notes included) and `files` (`{path, sessions}`).

use std::collections::HashSet;

use anyhow::{Context as _, bail};
use serde_json::json;
use tautly_core::compare::{
    BranchSummary, ForkPoint, MAX_SESSIONS, files_table, fork_cut, summarize_at,
};
use tautly_core::{Catalog, CliOverrides, Config, SessionReader, SessionRecord, printable};

use super::receipt::last_test;
use super::{Env, cost_cell, duration, open_ledger, thousands, usd};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The sessions to compare (ids, or prefixes of them): two to five.
    #[arg(required = true, num_args = 2.., value_name = "SESSION")]
    sessions: Vec<String>,
    /// Print one JSON object.
    #[arg(long)]
    json: bool,
}

pub fn run(args: Args, env: &Env) -> anyhow::Result<i32> {
    if args.sessions.len() > MAX_SESSIONS {
        eprintln!(
            "tautly: compare takes at most {MAX_SESSIONS} sessions ({} given)",
            args.sessions.len()
        );
        return Ok(2);
    }
    let mut found = Vec::with_capacity(args.sessions.len());
    let mut seen = HashSet::new();
    for id in &args.sessions {
        let s = env.find_session(id)?;
        if !seen.insert(s.id.clone()) {
            bail!("session {} is named more than once", printable(&s.id));
        }
        found.push(s);
    }
    let readers = found
        .iter()
        .map(|s| {
            SessionReader::<SessionRecord>::open(&s.path)
                .with_context(|| format!("cannot read session {}", printable(&s.id)))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let catalog = match Config::load(env.root(), CliOverrides::default()) {
        Ok(loaded) => loaded.config.effective_catalog(),
        Err(e) => {
            eprintln!(
                "tautly: the configuration could not be read ({e}); prices are the built-in ones"
            );
            Catalog::builtin()
        }
    };
    let ledger = match open_ledger(env) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "tautly: the cost ledger could not be read ({e:#}); costs are from the session logs"
            );
            None
        }
    };
    let fork = common_fork_point(&readers, env);
    let summaries = readers
        .iter()
        .zip(&found)
        .map(|(r, s)| {
            // With no shared fork point, each session's own records: a branch's copied
            // prefix is its parent's work (cp5 review C I4).
            let cut = match &fork {
                Some(f) => Some(f.offset),
                None => r.header().parent.as_ref().map(|(_, offset)| *offset),
            };
            summarize_at(r, ledger.as_ref(), &catalog, cut)
                .with_context(|| format!("session {}", printable(&s.id)))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let files = files_table(&summaries);
    if args.json {
        let files: Vec<_> = files
            .iter()
            .map(|(path, ix)| {
                json!({
                    "path": path,
                    "sessions": ix.iter().map(|&i| &summaries[i].session_id).collect::<Vec<_>>(),
                })
            })
            .collect();
        let out = json!({
            "fork_point": fork,
            "sessions": summaries,
            "files": files,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print!("{}", text(&summaries, fork.as_ref(), &files));
    }
    Ok(0)
}

/// The fork point all the sessions share: the first one's with each other one. Every one
/// lies on the first session's own chain, so the earliest (smallest offset) is the point
/// all of them hold. None when any pair has none: whole sessions are compared.
fn common_fork_point(readers: &[SessionReader<SessionRecord>], env: &Env) -> Option<ForkPoint> {
    let (first, rest) = readers.split_first()?;
    let mut common: Option<ForkPoint> = None;
    for r in rest {
        let fp = fork_cut(first.header(), r.header(), &env.data_home)?;
        if common.as_ref().is_none_or(|c| fp.offset < c.offset) {
            common = Some(fp);
        }
    }
    common
}

/// Each session's column name: its label, else the shortest prefix of its id (at least
/// eight characters) that no other compared session's id starts with. A label two
/// sessions share is followed by that prefix.
fn names(summaries: &[BranchSummary]) -> Vec<String> {
    let ids: Vec<&str> = summaries.iter().map(|s| s.session_id.as_str()).collect();
    let short = |s: &BranchSummary| {
        // By characters: an id read from a log may be any text.
        let id: Vec<char> = s.session_id.chars().collect();
        let prefix = |n: usize| id.iter().take(n).collect::<String>();
        let short = (8..id.len())
            .map(prefix)
            .find(|p| ids.iter().filter(|o| o.starts_with(p.as_str())).count() == 1)
            .unwrap_or_else(|| s.session_id.clone());
        printable(&short)
    };
    summaries
        .iter()
        .map(|s| match &s.label {
            Some(l)
                if summaries
                    .iter()
                    .filter(|o| o.label.as_ref() == Some(l))
                    .count()
                    > 1 =>
            {
                format!("{} ({})", printable(l), short(s))
            }
            Some(l) => printable(l),
            None => short(s),
        })
        .collect()
}

/// The text form.
fn text(
    summaries: &[BranchSummary],
    fork: Option<&ForkPoint>,
    files: &[(String, Vec<usize>)],
) -> String {
    let names = names(summaries);
    let row = |f: &dyn Fn(&BranchSummary) -> String| summaries.iter().map(f).collect();
    let rows: Vec<(&str, Vec<String>)> = vec![
        ("", names.clone()),
        ("session", row(&|s| printable(&s.session_id))),
        (
            "models",
            row(&|s| {
                let m: Vec<String> = s.models.iter().map(|m| printable(m.as_str())).collect();
                if m.is_empty() {
                    "-".into()
                } else {
                    m.join(", ")
                }
            }),
        ),
        ("user turns", row(&|s| s.user_turns.to_string())),
        ("steps", row(&|s| s.steps.to_string())),
        ("tokens in", row(&|s| thousands(s.tokens_in))),
        ("tokens out", row(&|s| thousands(s.tokens_out))),
        ("cache read", row(&|s| thousands(s.cache_read))),
        ("cost", row(&cost)),
        ("ledger cost", row(&|s| usd(s.ledger_usd))),
        ("wall time", row(&|s| duration(s.wall_ms))),
        ("tool calls", row(&|s| s.tool_calls.to_string())),
        ("blocked", row(&|s| s.blocked.to_string())),
        ("edit failures", row(&|s| s.edit_failures.to_string())),
        (
            "last test",
            row(&|s| {
                s.evidence
                    .as_ref()
                    .map_or_else(|| "none".to_string(), |e| last_test(Some(e)))
            }),
        ),
        ("files touched", row(&|s| s.files.len().to_string())),
        ("diff", row(&|s| format!("+{} -{}", s.added, s.removed))),
    ];
    let label_width = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let widths: Vec<usize> = (0..summaries.len())
        .map(|i| {
            rows.iter()
                .map(|(_, cells)| cells[i].chars().count())
                .max()
                .unwrap_or(0)
                + 2
        })
        .collect();
    let mut out = String::new();
    for (label, cells) in &rows {
        let mut line = format!("{label:<label_width$}");
        for (cell, w) in cells.iter().zip(&widths) {
            line.push_str(&pad(cell, *w));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push('\n');
    match fork {
        Some(f) => out.push_str(&format!(
            "figures count from the fork point: session {} at step {}\n",
            printable(&f.session_id),
            f.step
        )),
        None => out.push_str(
            "no common fork point: the figures cover each session's own records (a branch's \
             from its fork)\n",
        ),
    }
    out.push_str(
        "files and diff count tool edits only; changes made by commands are not in the logs\n",
    );
    out.push_str(
        "cost prices the logged usage at the current catalog; ledger cost is what the cost \
         ledger recorded, at the prices of the time (own records only)\n",
    );
    if summaries.iter().any(|s| s.cost_estimated) {
        out.push_str("(est.): some usage is the engine's estimate\n");
    }
    if summaries.iter().any(|s| s.cost_partial) {
        out.push_str(
            "?: some usage was on a model without a known price; the amount covers the priced part\n",
        );
    }
    if !files.is_empty() {
        out.push('\n');
        let rows: Vec<(String, String)> = files
            .iter()
            .map(|(path, ix)| (printable(path), touched_by(ix, &names)))
            .collect();
        let w = rows
            .iter()
            .map(|(p, _)| p.chars().count())
            .chain(std::iter::once("file".len()))
            .max()
            .unwrap_or(0)
            + 2;
        out.push_str(&format!("{}touched by\n", pad("file", w)));
        for (path, by) in rows {
            out.push_str(&format!("{}{by}\n", pad(&path, w)));
        }
    }
    let notes: Vec<String> = summaries
        .iter()
        .zip(&names)
        .flat_map(|(s, name)| {
            s.notes
                .iter()
                .map(move |n| format!("  {name}: {}", printable(n)))
        })
        .collect();
    if !notes.is_empty() {
        out.push_str("\nnotes:\n");
        for n in notes {
            out.push_str(&n);
            out.push('\n');
        }
    }
    out
}

/// `text` left-aligned in `width` characters.
fn pad(text: &str, width: usize) -> String {
    let n = text.chars().count();
    format!("{text}{}", " ".repeat(width.saturating_sub(n)))
}

/// Who touched a file: `only <x>`, `both` / `all`, or the sessions' names.
fn touched_by(ix: &[usize], names: &[String]) -> String {
    match ix {
        [one] => format!("only {}", names[*one]),
        _ if ix.len() == names.len() => if names.len() == 2 { "both" } else { "all" }.to_string(),
        _ => ix
            .iter()
            .map(|&i| names[i].as_str())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn cost(s: &BranchSummary) -> String {
    cost_cell(s.cost_usd, s.cost_partial, s.cost_estimated)
}
