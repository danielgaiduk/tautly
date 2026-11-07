//! `tautly replay <session> [--speed X] [--from-step N] [--output text|json]` (spec
//! 7.12.5): play a session back from its log. No provider is built, no key is read and no
//! tool runs; only the log and its blobs are read.
//!
//! Text output is the REPL's rendering (everything escaped for the terminal); `json`
//! prints the replayed events, one JSON object per line. Each pause is the logged gap
//! divided by `--speed` (default 1), at most 2 s; `--speed 0` prints at once. A log that
//! cannot be read to its end (a corrupt record) exits 1 after what could be replayed.

use std::io::Write as _;

use anyhow::{Context as _, bail};
use tautly_core::timetravel::step_at_offset;
use tautly_core::{CliOverrides, Config, Event, SessionReader, SessionRecord, replay};
use tautly_tui::ReplOptions;
use tautly_tui::render::{self, RenderState};

use super::Env;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Output {
    Text,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The session id, or a prefix of it.
    session: String,
    /// Playback speed: 2 is twice as fast, 0 instant.
    #[arg(long, default_value_t = 1.0, value_parser = parse_speed)]
    speed: f32,
    /// Start at step N (the numbers `tautly sessions show` gives).
    #[arg(long, value_name = "N")]
    from_step: Option<u32>,
    /// What to print: the REPL's rendering (default), or every event as JSON lines.
    #[arg(long, value_enum, default_value_t = Output::Text)]
    output: Output,
}

fn parse_speed(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(v) if replay::speed_ok(v) => Ok(v),
        _ => Err(format!(
            "`{s}` is not a speed (a number from 0 to {})",
            replay::MAX_SPEED
        )),
    }
}

pub async fn run(args: Args, env: &Env) -> anyhow::Result<i32> {
    let session = env.find_session(&args.session)?;
    let reader = SessionReader::<SessionRecord>::open(&session.path)
        .with_context(|| format!("cannot read session {}", session.id))?;
    if let Some(at) = args.from_step {
        let last = step_at_offset(&reader, u64::MAX);
        if at > last {
            bail!(
                "there is no step {at} in session {}; its last step is {last}",
                session.id
            );
        }
    }
    let catalog = Config::load(env.root(), CliOverrides::default())?
        .config
        .effective_catalog();
    let opts = ReplOptions {
        color: false,
        footer: false,
        ..ReplOptions::default()
    };
    let mut st = RenderState::default();
    let mut stdout = std::io::stdout();
    let mut prev = None;
    let mut events = replay::events(&reader, args.from_step).priced(catalog);
    for (offset, t_ms, event) in events.by_ref() {
        if let Some(p) = prev {
            let gap = replay::delay(p, t_ms, args.speed);
            if !gap.is_zero() {
                stdout.flush()?;
                tokio::time::sleep(gap).await;
            }
        }
        prev = Some(t_ms);
        match args.output {
            Output::Json => writeln!(stdout, "{}", serde_json::to_string(&event)?)?,
            Output::Text => {
                let wrapped = Event::Replayed {
                    offset,
                    t_ms,
                    event: Box::new(event),
                };
                if let Some(text) = render::event(&wrapped, &mut st, &opts) {
                    stdout.write_all(text.as_bytes())?;
                }
            }
        }
    }
    if args.output == Output::Text {
        stdout.write_all(st.break_line().as_bytes())?;
    }
    stdout.flush()?;
    // The log broke off (its warning was the last event): not a complete replay.
    if events.truncated() {
        eprintln!("tautly: the session log could not be read to its end");
        return Ok(1);
    }
    Ok(0)
}
