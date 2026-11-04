//! `tautly stats` (spec 7.11.1): spend and savings from the cost ledger, grouped by day,
//! model or project. Tokens and cost come from the ledger's usage rows; turns, savings and
//! wall time from its receipt rows. Savings are estimates and labelled so.

use std::time::{Duration, SystemTime};

use clap::ValueEnum;
use serde_json::json;
use tautly_core::{GroupBy, StatsRow, printable};

use super::{Env, duration, open_ledger, percent, thousands, usd};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// How far back to look.
    #[arg(long, value_enum, default_value = "30d")]
    since: Since,
    /// How to group the rows.
    #[arg(long, value_enum, default_value = "day")]
    by: By,
    /// Print JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Since {
    #[value(name = "7d")]
    Week,
    #[value(name = "30d")]
    Month,
    All,
}

impl Since {
    fn name(self) -> &'static str {
        match self {
            Since::Week => "7d",
            Since::Month => "30d",
            Since::All => "all",
        }
    }

    fn start(self) -> Option<SystemTime> {
        let days = match self {
            Since::Week => 7,
            Since::Month => 30,
            Since::All => return None,
        };
        SystemTime::now().checked_sub(Duration::from_secs(days * 86_400))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum By {
    Day,
    Model,
    Project,
}

impl By {
    fn name(self) -> &'static str {
        match self {
            By::Day => "day",
            By::Model => "model",
            By::Project => "project",
        }
    }

    fn group(self) -> GroupBy {
        match self {
            By::Day => GroupBy::Day,
            By::Model => GroupBy::Model,
            By::Project => GroupBy::Project,
        }
    }
}

pub fn run(args: Args, env: &Env) -> anyhow::Result<i32> {
    let rows = match open_ledger(env)? {
        Some(ledger) => ledger.stats(args.since.start(), args.by.group())?,
        None => Vec::new(),
    };
    if args.json {
        let out = json!({
            "since": args.since.name(),
            "by": args.by.name(),
            "rows": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    if rows.is_empty() {
        println!("no usage recorded in this period");
        return Ok(0);
    }
    print!("{}", table(&rows, args.by));
    Ok(0)
}

/// The rows as an aligned table, with a total and a legend.
fn table(rows: &[StatsRow], by: By) -> String {
    let header = [
        by.name().to_uppercase(),
        "TURNS".into(),
        "TOKENS".into(),
        "COST".into(),
        "SAVED est.".into(),
        "SAVED $ est.".into(),
        "WALL".into(),
        "CACHE HIT".into(),
    ];
    let mut lines = vec![header.to_vec()];
    for r in rows {
        lines.push(cells(&printable(&r.key), r));
    }
    lines.push(cells("total", &total(rows)));
    let widths: Vec<usize> = (0..header.len())
        .map(|c| {
            lines
                .iter()
                .map(|l| l[c].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for line in &lines {
        let mut text = String::new();
        for (c, cell) in line.iter().enumerate() {
            let pad = widths[c] - cell.chars().count();
            if c == 0 {
                text.push_str(cell);
                text.push_str(&" ".repeat(pad));
            } else {
                text.push_str("  ");
                text.push_str(&" ".repeat(pad));
                text.push_str(cell);
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out.push_str(
        "\nest.: savings are estimates (bytes / 4) against a plain harness with the same \
         30 KB output cap\n",
    );
    if rows.iter().any(|r| r.estimated > 0) {
        out.push_str("*: includes usage the engine estimated (a response that reported none)\n");
    }
    if rows.iter().any(|r| r.unpriced > 0) {
        out.push_str("?: includes usage of a model without a known price (not in COST)\n");
    }
    out
}

fn cells(key: &str, r: &StatsRow) -> Vec<String> {
    let mut tokens = thousands(r.tokens);
    if r.estimated > 0 {
        tokens.push('*');
    }
    let mut cost = usd(r.cost_usd);
    if r.unpriced > 0 {
        cost.push('?');
    }
    vec![
        key.to_string(),
        thousands(r.turns),
        tokens,
        cost,
        thousands(r.saved_tokens),
        usd(r.saved_usd),
        duration(r.wall_ms),
        percent(r.cache_hit_ratio),
    ]
}

/// All rows summed (the cache hit ratio is left out: the rows do not carry its parts).
fn total(rows: &[StatsRow]) -> StatsRow {
    let sum_opt = |f: fn(&StatsRow) -> Option<f64>| {
        rows.iter()
            .filter_map(f)
            .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
    };
    StatsRow {
        key: String::new(),
        turns: rows.iter().map(|r| r.turns).sum(),
        tokens: rows.iter().map(|r| r.tokens).sum(),
        cost_usd: sum_opt(|r| r.cost_usd),
        unpriced: rows.iter().map(|r| r.unpriced).sum(),
        estimated: rows.iter().map(|r| r.estimated).sum(),
        saved_tokens: rows.iter().map(|r| r.saved_tokens).sum(),
        saved_usd: sum_opt(|r| r.saved_usd),
        wall_ms: rows.iter().map(|r| r.wall_ms).sum(),
        cache_hit_ratio: None,
    }
}
