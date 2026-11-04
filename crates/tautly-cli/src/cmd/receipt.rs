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

/// What `?` after an amount means.
const UNPRICED_NOTE: &str =
    "some turns used a model without a known price; the amount covers the priced ones";

pub fn run(args: Args, env: &Env) -> anyhow::Result<i32> {
    let ledger = open_ledger(env)?;
    let receipts_of = |id: &str| -> anyhow::Result<Vec<StoredReceipt>> {
        Ok(match &ledger {
            Some(l) => l.session_receipts(id)?,
            None => Vec::new(),
        })
    };
    let no_receipts = |id: &str| {
        anyhow::anyhow!(
            "session {id} has no receipts (it has not finished a turn, ran before receipts \
             were recorded, or the cost ledger was unavailable)"
        )
    };
    // `--last` only says the default out loud.
    let (session, receipts) = match &args.session {
        Some(id) => {
            let session = env.find_session(id)?;
            let receipts = receipts_of(&session.id)?;
            if receipts.is_empty() {
                return Err(no_receipts(&session.id));
            }
            (session, receipts)
        }
        None => {
            let Some(newest) = env.latest_session()? else {
                bail!("no session in this work tree yet");
            };
            let receipts = receipts_of(&newest.id)?;
            if receipts.is_empty() {
                // A session that just started (or broke off in its first turn) has none
                // yet: show the newest one that has.
                let mut found = None;
                for s in tautly_core::list_logs_here(&env.data_home, env.root())? {
                    let r = receipts_of(&s.id)?;
                    if !r.is_empty() {
                        found = Some((s, r));
                        break;
                    }
                }
                let Some(found) = found else {
                    return Err(no_receipts(&newest.id));
                };
                eprintln!(
                    "tautly: the newest session {} has no receipts yet; showing {} (choose \
                     one with --session ID)",
                    newest.id, found.0.id
                );
                found
            } else {
                (newest, receipts)
            }
        }
    };
    let evidence = last_evidence(&session)?;
    let summary = Summary::of(&receipts);
    let rows = rows(&summary, evidence.as_ref());
    let unpriced = summary.cost_usd.partial || summary.total_usd.partial;
    if args.markdown {
        println!("| Tautly receipt | |");
        println!("|---|---|");
        for (label, value) in rows {
            println!("| {} | {} |", cell(label), cell(&value));
        }
        println!();
        println!("est. = estimate (bytes / 4, against a plain harness with the same 30 KB cap)");
        if unpriced {
            println!("? = {UNPRICED_NOTE}");
        }
    } else {
        println!("{:<14} {}", "session", session.id);
        for (label, value) in rows {
            println!("{:<14} {value}", label.to_lowercase());
        }
        if unpriced {
            println!("\n?: {UNPRICED_NOTE}");
        }
    }
    Ok(0)
}

/// A sum of amounts some of which may be unknown (an unpriced model).
#[derive(Default, Clone, Copy)]
struct Priced {
    /// The known amounts summed; `None` when none was known.
    sum: Option<f64>,
    /// Some amount was unknown.
    partial: bool,
}

impl Priced {
    fn add(&mut self, v: Option<f64>) {
        match v {
            Some(v) => self.sum = Some(self.sum.unwrap_or(0.0) + v),
            None => self.partial = true,
        }
    }

    /// The amount, with `?` when it covers only the priced part.
    fn show(self) -> String {
        let mut s = usd(self.sum);
        if self.partial && self.sum.is_some() {
            s.push('?');
        }
        s
    }
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
    cost_usd: Priced,
    estimated: bool,
    index: u64,
    diff_reads: u64,
    context_gc: u64,
    filters: u64,
    cache_usd: Priced,
    total_usd: Priced,
}

impl Summary {
    fn of(receipts: &[StoredReceipt]) -> Summary {
        let mut s = Summary::default();
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
            s.cost_usd.add(r.cost_usd);
            s.estimated |= r.estimated;
            s.index += r.saved.index;
            s.diff_reads += r.saved.diff_reads;
            s.context_gc += r.saved.context_gc;
            s.filters += r.saved.filters;
            s.cache_usd.add(r.saved.cache_usd);
            s.total_usd.add(r.saved.total_usd);
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
        ("Cost", format!("{}{est}", s.cost_usd.show())),
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
            format!(
                "{} (prompt cache {})",
                s.total_usd.show(),
                s.cache_usd.show()
            ),
        ),
        ("Last test", last_test(evidence)),
    ]
}

/// The last turn's test evidence, without its command (which may name paths).
pub(super) fn last_test(evidence: Option<&TestEvidence>) -> String {
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
