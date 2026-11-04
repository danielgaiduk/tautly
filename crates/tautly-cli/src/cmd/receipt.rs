//! `tautly receipt` (spec 7.11.1): one session's turns, tool calls, wall time, tokens,
//! cache hit ratio, cost, savings estimates and the last turn's test evidence, from the
//! cost ledger's receipt rows and the session log. It never shows paths, prompts, code or
//! the project's name, so it can be pasted anywhere (`--markdown` prints a table).

use std::collections::BTreeSet;

use anyhow::{Context as _, bail};
use tautly_core::{
    LogSummary, SessionReader, SessionRecord, StoredReceipt, TestEvidence, printable,
};

use super::{Env, duration, open_ledger, percent, thousands, usd};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The most recent session of this project (the default).
    #[arg(long, conflicts_with = "session")]
    last: bool,
    /// The session id, or a prefix of it.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Print a Markdown table to paste.
    #[arg(long)]
    markdown: bool,
}

pub fn run(args: Args, env: &Env) -> anyhow::Result<i32> {
    // `--last` only says the default out loud.
    let session = match &args.session {
        Some(id) => env.find_session(id)?,
        None => match env.latest_session()? {
            Some(s) => s,
            None => bail!("no session in this work tree yet"),
        },
    };
    let receipts = match open_ledger(env)? {
        Some(ledger) => ledger.session_receipts(&session.id)?,
        None => Vec::new(),
    };
    if receipts.is_empty() {
        bail!(
            "session {} has no receipts (it ran before receipts were recorded, or the cost \
             ledger was unavailable)",
            session.id
        );
    }
    let evidence = last_evidence(&session)?;
    let rows = rows(&Summary::of(&receipts), evidence.as_ref());
    if args.markdown {
        println!("| Tautly receipt | |");
        println!("|---|---|");
        for (label, value) in rows {
            println!("| {} | {} |", cell(label), cell(&value));
        }
        println!();
        println!("est. = estimate (bytes / 4, against a plain harness with the same 30 KB cap)");
    } else {
        println!("{:<14} {}", "session", session.id);
        for (label, value) in rows {
            println!("{:<14} {value}", label.to_lowercase());
        }
    }
    Ok(0)
}

/// A Markdown table cell: a `|` would end it.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// The evidence of the session's last finished turn (none: it ran no tests).
fn last_evidence(session: &LogSummary) -> anyhow::Result<Option<TestEvidence>> {
    let reader = SessionReader::<SessionRecord>::open(&session.path)
        .with_context(|| format!("cannot read session {}", session.id))?;
    let mut last = None;
    for rec in reader.records() {
        if let (_, SessionRecord::TurnEnd { evidence, .. }) = rec? {
            last = evidence;
        }
    }
    Ok(last)
}

/// The session's receipts summed.
#[derive(Default)]
struct Summary {
    models: BTreeSet<String>,
    turns: usize,
    tool_calls: u64,
    wall_ms: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    cache_write: u64,
    side_tokens: u64,
    cost_usd: Option<f64>,
    estimated: bool,
    index: u64,
    diff_reads: u64,
    context_gc: u64,
    filters: u64,
    cache_usd: Option<f64>,
    total_usd: Option<f64>,
}

impl Summary {
    fn of(receipts: &[StoredReceipt]) -> Summary {
        // A sum of amounts is known only when every one is.
        let add = |acc: Option<f64>, v: Option<f64>| acc.zip(v).map(|(a, b)| a + b);
        let mut s = Summary {
            cost_usd: Some(0.0),
            cache_usd: Some(0.0),
            total_usd: Some(0.0),
            ..Summary::default()
        };
        for r in receipts {
            s.models.insert(r.model.clone());
            s.turns += 1;
            s.tool_calls += u64::from(r.tool_calls);
            s.wall_ms += r.wall_ms;
            s.tokens_in += r.tokens_in;
            s.tokens_out += r.tokens_out;
            s.cache_read += r.cache_read;
            s.cache_write += r.cache_write;
            s.side_tokens += r.side_tokens;
            s.cost_usd = add(s.cost_usd, r.cost_usd);
            s.estimated |= r.estimated;
            s.index += r.saved.index;
            s.diff_reads += r.saved.diff_reads;
            s.context_gc += r.saved.context_gc;
            s.filters += r.saved.filters;
            s.cache_usd = add(s.cache_usd, r.saved.cache_usd);
            s.total_usd = add(s.total_usd, r.saved.total_usd);
        }
        s
    }
}

/// The receipt's `(label, value)` lines.
fn rows(s: &Summary, evidence: Option<&TestEvidence>) -> Vec<(&'static str, String)> {
    let est = if s.estimated { " (est.)" } else { "" };
    let seen = s.tokens_in + s.cache_read;
    let models: Vec<String> = s.models.iter().map(|m| printable(m)).collect();
    let saved = s.index + s.diff_reads + s.context_gc + s.filters;
    vec![
        ("Model", models.join(", ")),
        ("Turns", s.turns.to_string()),
        ("Tool calls", thousands(s.tool_calls)),
        ("Wall time", duration(s.wall_ms)),
        (
            "Tokens",
            format!(
                "in {} · out {} · cache read {} · cache write {}{est}",
                thousands(s.tokens_in),
                thousands(s.tokens_out),
                thousands(s.cache_read),
                thousands(s.cache_write),
            ),
        ),
        ("Side model", format!("{} tokens", thousands(s.side_tokens))),
        (
            "Cache hit",
            percent((seen > 0).then(|| s.cache_read as f64 / seen as f64)),
        ),
        ("Cost", format!("{}{est}", usd(s.cost_usd))),
        (
            "Saved (est.)",
            format!(
                "{} tokens: index {} · diff reads {} · context GC {} · filters {}",
                thousands(saved),
                thousands(s.index),
                thousands(s.diff_reads),
                thousands(s.context_gc),
                thousands(s.filters),
            ),
        ),
        (
            "Saved $ (est.)",
            format!("{} (prompt cache {})", usd(s.total_usd), usd(s.cache_usd)),
        ),
        ("Last test", last_test(evidence)),
    ]
}

/// The last turn's test evidence, without its command (which may name paths).
fn last_test(evidence: Option<&TestEvidence>) -> String {
    let Some(e) = evidence else {
        return "none in the last turn".to_string();
    };
    let mut parts = vec![match e.exit_code {
        Some(code) => format!("exit {code}"),
        None => "killed".to_string(),
    }];
    if let (Some(passed), Some(failed)) = (e.passed, e.failed) {
        parts.push(format!("{passed} passed, {failed} failed"));
    }
    parts.push(if e.stale {
        format!(
            "stale ({} edits, {} commands since)",
            e.edits_since, e.execs_since
        )
    } else {
        "fresh".to_string()
    });
    parts.join(" · ")
}
