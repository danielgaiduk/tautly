//! The REPL loop: read a line, send it as a message or slash command, print the events
//! that answer it, ask the user what the engine asks.
//!
//! Keys (spec 7.11.8): during a turn (or any command the REPL waits on) a SIGINT cancels
//! it; another Ctrl+C within 2 s of that exits, whether it comes as a SIGINT or at the
//! prompt. At the prompt Ctrl+C otherwise just clears the line, and Ctrl+D exits.

use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures::{FutureExt as _, Stream, StreamExt as _};
use serde_json::Value;
use tautly_core::{Command, Engine, Event, ModelId, printable};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;

use crate::approve::{self, ToolAnswer};
use crate::input::HistoryFile;
use crate::render::{self, Receipt, RenderState};
use crate::slash::{self, HELP, Slash};
use crate::{EngineFactory, Line, LineSource, ReplError, ReplOptions};

/// The prompt for messages and slash commands.
const PROMPT: &str = "> ";
/// A second Ctrl+C this soon after one that cancelled exits.
const DOUBLE_INTERRUPT: Duration = tautly_core::signals::DOUBLE_INTERRUPT;
/// How long the REPL waits for an answer to a command that may send none (`/pin`).
const QUIET: Duration = Duration::from_millis(200);
/// Sessions `/resume` lists.
const RESUME_LIST: usize = 10;

/// Run the REPL until Ctrl+D (or a second Ctrl+C within 2 s of one that cancelled), then
/// shut the session down and wait for its last event. `signals` yields one item per
/// SIGINT: during a turn it cancels.
pub async fn run_repl(
    factory: EngineFactory,
    input: impl LineSource,
    out: impl AsyncWrite + Unpin,
    signals: impl Stream<Item = ()>,
    opts: ReplOptions,
) -> Result<(), ReplError> {
    let factory = Arc::new(factory);
    let (engine, events) = build(&factory, None).await?;
    let root = tautly_core::session_root(&opts.root).unwrap_or_else(|_| opts.root.clone());
    let mut st = RenderState::default();
    st.model = opts.model.clone();
    let mut repl = Repl {
        factory,
        input,
        out,
        signals: Box::pin(signals),
        history: HistoryFile::new(opts.history.clone()),
        opts,
        st,
        engine,
        events,
        root,
        last_interrupt: None,
        answered: HashSet::new(),
        listing: Vec::new(),
    };
    // A session resumed at start (`--continue`) counts its earlier turns in the total.
    let first = repl.engine.session_id().to_string();
    repl.st.receipts = repl.earlier_receipts(&first).await;
    let ended = repl.main_loop().await;
    let closed = repl.close().await;
    ended.and(closed)
}

/// Build a session off the async workers (the factory may block).
async fn build(
    factory: &Arc<EngineFactory>,
    resume: Option<String>,
) -> Result<(Engine, mpsc::Receiver<Event>), ReplError> {
    let factory = Arc::clone(factory);
    let built = tokio::task::spawn_blocking(move || factory(resume))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(built?)
}

/// How waiting on the engine ended.
enum Pumped {
    /// The event that was waited for.
    Done(Box<Event>),
    /// The user asked to exit, or the session stopped.
    Exit,
}

struct Repl<I, W, S> {
    factory: Arc<EngineFactory>,
    input: I,
    out: W,
    signals: Pin<Box<S>>,
    history: HistoryFile,
    opts: ReplOptions,
    st: RenderState,
    engine: Engine,
    events: mpsc::Receiver<Event>,
    /// The work tree's root, as sessions are listed by.
    root: PathBuf,
    /// When a Ctrl+C last cancelled something.
    last_interrupt: Option<Instant>,
    /// Approval requests already answered (a pending MCP request is announced again at
    /// each turn start).
    answered: HashSet<String>,
    /// The session ids `/resume` listed last, newest first.
    listing: Vec<String>,
}

impl<I, W, S> Repl<I, W, S>
where
    I: LineSource,
    W: AsyncWrite + Unpin,
    S: Stream<Item = ()>,
{
    async fn main_loop(&mut self) -> Result<(), ReplError> {
        let hint = "type a message · /help for commands · Ctrl+D to exit";
        let hint = self.dim(hint);
        self.print_line(&hint).await?;
        loop {
            if self.drain_ready().await? {
                return Ok(());
            }
            self.start_line().await?;
            let line = self.input.read_line(PROMPT).await?;
            self.st.typed();
            let text = match line {
                Line::CtrlD => return Ok(()),
                Line::CtrlC if self.recently_interrupted() => return Ok(()),
                Line::CtrlC => continue,
                Line::Text(t) => t,
            };
            if text.trim().is_empty() {
                continue;
            }
            self.history.add(&text);
            let exit = match slash::parse(&text) {
                Some(cmd) => self.slash(cmd).await?,
                None => {
                    self.send(Command::SendMessage { text }).await?;
                    matches!(
                        self.pump(|e| matches!(e, Event::TurnFinished { .. }))
                            .await?,
                        Pumped::Exit
                    )
                }
            };
            if exit {
                return Ok(());
            }
        }
    }

    /// Shut the session down and print its last events.
    async fn close(&mut self) -> Result<(), ReplError> {
        let _ = self.engine.send(Command::Shutdown).await;
        while let Some(ev) = self.events.recv().await {
            if let Some(text) = render::event(&ev, &mut self.st, &self.opts) {
                self.write(&text).await?;
            }
        }
        Ok(())
    }

    async fn send(&mut self, cmd: Command) -> Result<(), ReplError> {
        Ok(self.engine.send(cmd).await?)
    }

    /// Handle the events already queued (warnings, a late MCP approval) before the
    /// prompt. True when the user asked to exit meanwhile.
    async fn drain_ready(&mut self) -> Result<bool, ReplError> {
        while let Ok(ev) = self.events.try_recv() {
            if self.handle(ev).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Handle events until one that `until` accepts (returned), a Ctrl+C exit or the end
    /// of the session. A SIGINT cancels what runs.
    async fn pump(&mut self, until: impl Fn(&Event) -> bool) -> Result<Pumped, ReplError> {
        // A SIGINT from before the wait is not about what runs now.
        while let Some(Some(())) = self.signals.next().now_or_never() {}
        loop {
            let ev = tokio::select! {
                // A Ctrl+C is acted on before events already queued behind it.
                biased;
                Some(()) = self.signals.next() => {
                    if self.interrupt().await? {
                        return Ok(Pumped::Exit);
                    }
                    continue;
                }
                ev = self.events.recv() => ev,
            };
            let Some(ev) = ev else {
                return Ok(Pumped::Exit);
            };
            let done = until(&ev);
            let answer = done.then(|| ev.clone());
            if self.handle(ev).await? {
                return Ok(Pumped::Exit);
            }
            if let Some(ev) = answer {
                return Ok(Pumped::Done(Box::new(ev)));
            }
        }
    }

    /// Handle events for as long as they keep coming within [`QUIET`].
    async fn quiet(&mut self) -> Result<bool, ReplError> {
        loop {
            let ev = tokio::select! {
                ev = self.events.recv() => ev,
                Some(()) = self.signals.next() => {
                    if self.interrupt().await? {
                        return Ok(true);
                    }
                    continue;
                }
                () = tokio::time::sleep(QUIET) => return Ok(false),
            };
            match ev {
                Some(ev) => {
                    if self.handle(ev).await? {
                        return Ok(true);
                    }
                }
                None => return Ok(true),
            }
        }
    }

    /// A Ctrl+C while something runs: cancel it, or exit when one came within 2 s.
    async fn interrupt(&mut self) -> Result<bool, ReplError> {
        if self.recently_interrupted() {
            return Ok(true);
        }
        self.last_interrupt = Some(Instant::now());
        self.send(Command::Cancel).await?;
        Ok(false)
    }

    fn recently_interrupted(&self) -> bool {
        self.last_interrupt
            .is_some_and(|t| t.elapsed() <= DOUBLE_INTERRUPT)
    }

    /// Print one event, asking the user when the engine waits for an answer. True when
    /// the user asked to exit.
    async fn handle(&mut self, ev: Event) -> Result<bool, ReplError> {
        match &ev {
            Event::ToolApprovalRequested {
                request_id,
                tool,
                args,
                ..
            } => {
                if !self.answered.insert(request_id.clone()) {
                    return Ok(false);
                }
                let (request_id, rule) = (request_id.clone(), self.exact_rule(tool, args));
                self.print_event(&ev).await?;
                self.ask_tool(request_id, rule).await
            }
            Event::McpServerApprovalRequested { request_id, .. } => {
                if !self.answered.insert(request_id.clone()) {
                    return Ok(false);
                }
                let request_id = request_id.clone();
                self.print_event(&ev).await?;
                let (yes, exit) = self.ask_yes_no("start this server? [y]es [n]o ").await?;
                let cmd = if yes {
                    Command::Approve {
                        request_id,
                        session_rule: None,
                    }
                } else {
                    Command::Deny {
                        request_id,
                        reason: None,
                    }
                };
                self.send(cmd).await?;
                Ok(exit)
            }
            Event::MemoryProposed { proposal_id, .. } => {
                if !self.answered.insert(proposal_id.clone()) {
                    return Ok(false);
                }
                let proposal_id = proposal_id.clone();
                self.print_event(&ev).await?;
                let (yes, exit) = self.ask_yes_no("save to memory? [y]es [n]o ").await?;
                let cmd = if yes {
                    Command::ApproveMemory { proposal_id }
                } else {
                    Command::RejectMemory { proposal_id }
                };
                self.send(cmd).await?;
                Ok(exit)
            }
            _ => {
                self.print_event(&ev).await?;
                Ok(false)
            }
        }
    }

    /// The exact-target rule for a call (paths judged against the session's root).
    fn exact_rule(&self, tool: &str, args: &Value) -> Option<String> {
        approve::exact_rule(&self.root, tool, args)
    }

    /// Ask `allow? [y]es [n]o [a]lways this session (<rule>)` and send the answer.
    async fn ask_tool(
        &mut self,
        request_id: String,
        rule: Option<String>,
    ) -> Result<bool, ReplError> {
        let prompt = approve::tool_prompt(rule.as_deref());
        loop {
            self.start_line().await?;
            let line = self.input.read_answer(&prompt).await?;
            self.st.typed();
            let (cmd, exit) = match line {
                Line::Text(answer) => match approve::tool_answer(&answer, rule.as_deref()) {
                    ToolAnswer::Once => (approve_cmd(&request_id, None), false),
                    ToolAnswer::Always(r) => (approve_cmd(&request_id, Some(r)), false),
                    ToolAnswer::Deny => (deny_cmd(&request_id), false),
                    ToolAnswer::Again(why) => {
                        self.print_line(why).await?;
                        continue;
                    }
                },
                Line::CtrlD => (deny_cmd(&request_id), false),
                // Ctrl+C at the question denies the call and cancels the turn.
                Line::CtrlC => {
                    self.send(deny_cmd(&request_id)).await?;
                    return self.interrupt().await;
                }
            };
            self.send(cmd).await?;
            return Ok(exit);
        }
    }

    /// Ask a `y`/`n` question: (yes, the user asked to exit). Ctrl+C or Ctrl+D is no.
    async fn ask_yes_no(&mut self, prompt: &str) -> Result<(bool, bool), ReplError> {
        loop {
            self.start_line().await?;
            let line = self.input.read_answer(prompt).await?;
            self.st.typed();
            match line {
                Line::Text(a) => match approve::yes_no(&a) {
                    Some(yes) => return Ok((yes, false)),
                    None => self.print_line("answer y or n").await?,
                },
                Line::CtrlD => return Ok((false, false)),
                Line::CtrlC => return Ok((false, self.recently_interrupted())),
            }
        }
    }

    /// Run a slash command. True when the user asked to exit meanwhile.
    async fn slash(&mut self, cmd: Slash) -> Result<bool, ReplError> {
        let exit = |p: Pumped| matches!(p, Pumped::Exit);
        match cmd {
            Slash::Model(None) => self.list_models().await?,
            Slash::Model(Some(m)) => {
                self.send(Command::SetModel {
                    model: ModelId::from(m.as_str()),
                })
                .await?;
                return Ok(exit(
                    self.pump(|e| matches!(e, Event::ModelChanged { .. } | Event::Error { .. }))
                        .await?,
                ));
            }
            Slash::Cost => {
                let text = render::cost(&self.st);
                self.print_line(&text).await?;
            }
            Slash::Diff(turn) => {
                self.send(Command::Diff { turn }).await?;
                return Ok(exit(
                    self.pump(|e| matches!(e, Event::SessionDiff { .. } | Event::Error { .. }))
                        .await?,
                ));
            }
            Slash::Undo(turn) => return self.undo(turn).await,
            Slash::Resume(None) => self.list_sessions().await?,
            Slash::Resume(Some(which)) => return self.resume(&which).await,
            Slash::Compact => {
                self.send(Command::Compact).await?;
                return Ok(exit(
                    self.pump(|e| {
                        matches!(
                            e,
                            Event::ContextStatus { .. }
                                | Event::Warning { .. }
                                | Event::Error { .. }
                        )
                    })
                    .await?,
                ));
            }
            Slash::Pin(text) => {
                self.send(Command::Pin { text }).await?;
                return self.quiet().await;
            }
            Slash::Unpin(text) => {
                self.send(Command::Unpin { text }).await?;
                return self.quiet().await;
            }
            Slash::Fork => {
                self.send(Command::Fork {
                    at: None,
                    label: None,
                })
                .await?;
                return Ok(exit(
                    self.pump(|e| matches!(e, Event::SessionChanged { .. } | Event::Error { .. }))
                        .await?,
                ));
            }
            Slash::MemoryConsolidate => {
                self.send(Command::ConsolidateMemory).await?;
                return Ok(exit(
                    self.pump(|e| {
                        matches!(
                            e,
                            Event::MemoryProposed { .. }
                                | Event::Warning { .. }
                                | Event::Error { .. }
                        )
                    })
                    .await?,
                ));
            }
            Slash::Help => self.print_line(HELP).await?,
            Slash::Unknown(what) => {
                let text = format!("unknown command: {}\n{HELP}", printable(&what));
                self.print_line(&text).await?;
            }
        }
        Ok(false)
    }

    /// `/undo [N]`: the dry run, then (on `y`) the undo.
    async fn undo(&mut self, turn: Option<u32>) -> Result<bool, ReplError> {
        let answered = |e: &Event| matches!(e, Event::Undone { .. } | Event::Error { .. });
        self.send(Command::Undo {
            turn,
            dry_run: true,
        })
        .await?;
        let shown = match self.pump(answered).await? {
            Pumped::Exit => return Ok(true),
            Pumped::Done(ev) => ev,
        };
        if !matches!(*shown, Event::Undone { dry_run: true, .. }) {
            return Ok(false);
        }
        let (yes, exit) = self.ask_yes_no("undo? [y]es [n]o ").await?;
        if exit || !yes {
            return Ok(exit);
        }
        self.send(Command::Undo {
            turn,
            dry_run: false,
        })
        .await?;
        Ok(matches!(self.pump(answered).await?, Pumped::Exit))
    }

    /// `/model`: the current model, then the others whose provider has a key.
    async fn list_models(&mut self) -> Result<(), ReplError> {
        let current = self.st.model.clone().unwrap_or_else(|| "unknown".into());
        let others = match self.opts.models.clone() {
            Some(lister) => tokio::task::spawn_blocking(move || lister())
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut text = format!("* {} (current)\n", printable(&current));
        for m in others.iter().filter(|m| **m != current) {
            text.push_str(&format!("  {}\n", printable(m)));
        }
        text.push_str("switch with /model <id>");
        self.print_line(&text).await
    }

    /// `/resume`: the work tree's last sessions, newest first.
    async fn list_sessions(&mut self) -> Result<(), ReplError> {
        let found = self.sessions().await;
        if found.is_empty() {
            return self
                .print_line("no earlier session in this work tree")
                .await;
        }
        let now = SystemTime::now();
        let mut text = String::new();
        for (i, s) in found.iter().enumerate() {
            let age = now.duration_since(s.started).unwrap_or_default();
            let cost = s
                .cost_usd
                .map_or_else(|| "-".to_string(), |c| format!("${c:.4}"));
            text.push_str(&format!(
                "{:>2}  {}  {:>8}  {}  {cost}\n",
                i + 1,
                printable(&s.id),
                ago(age),
                printable(&s.first_prompt)
            ));
        }
        text.push_str("resume with /resume <n> or /resume <id>");
        self.listing = found.into_iter().map(|s| s.id).collect();
        self.print_line(&text).await
    }

    async fn sessions(&self) -> Vec<tautly_core::sessions::SessionSummary> {
        let Some(home) = self.opts.data_home.clone() else {
            return Vec::new();
        };
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            tautly_core::sessions::list_in(&home, &root, RESUME_LIST)
        })
        .await
        .unwrap_or_default()
    }

    /// `/resume <n|id>`: shut this session down, then resume that one.
    async fn resume(&mut self, which: &str) -> Result<bool, ReplError> {
        let id = match which.parse::<usize>() {
            Ok(n) if (1..=RESUME_LIST).contains(&n) => {
                if self.listing.is_empty() {
                    self.listing = self.sessions().await.into_iter().map(|s| s.id).collect();
                }
                match self.listing.get(n - 1) {
                    Some(id) => id.clone(),
                    None => {
                        self.print_line(&format!("there is no session {n} in the list"))
                            .await?;
                        return Ok(false);
                    }
                }
            }
            _ => which.to_string(),
        };
        self.close().await?;
        let (engine, events) = match build(&self.factory, Some(id.clone())).await {
            Ok(built) => built,
            Err(e) => {
                let text = format!(
                    "cannot resume {}: {e}; starting a new session",
                    printable(&id)
                );
                self.print_line(&text).await?;
                self.st.new_session();
                build(&self.factory, None).await?
            }
        };
        self.engine = engine;
        self.events = events;
        self.answered.clear();
        self.st.new_session();
        self.st.receipts = self.earlier_receipts(&id).await;
        self.print_line(&format!("resumed {}", printable(&id)))
            .await?;
        Ok(false)
    }

    /// The receipts the cost ledger holds for session `id` (its earlier runs).
    async fn earlier_receipts(&self, id: &str) -> Vec<Receipt> {
        let Some(home) = self.opts.data_home.clone() else {
            return Vec::new();
        };
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let file = home.join(tautly_core::ledger::LEDGER_FILE);
            if !file.exists() {
                return Vec::new();
            }
            let Ok(rows) = tautly_core::Ledger::open(&file).and_then(|l| l.session_receipts(&id))
            else {
                return Vec::new();
            };
            rows.into_iter()
                .map(|r| Receipt {
                    turn: r.turn,
                    steps: r.steps,
                    tool_calls: r.tool_calls,
                    wall_ms: r.wall_ms,
                    tokens_in: r.tokens_in,
                    tokens_out: r.tokens_out,
                    cache_hit_ratio: (r.tokens_in + r.cache_read > 0)
                        .then(|| r.cache_read as f64 / (r.tokens_in + r.cache_read) as f64),
                    cost_usd: r.cost_usd,
                    saved_tokens: render::saved_tokens(&r.saved),
                    estimated: r.estimated,
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    async fn print_event(&mut self, ev: &Event) -> Result<(), ReplError> {
        match render::event(ev, &mut self.st, &self.opts) {
            Some(text) => self.write(&text).await,
            None => Ok(()),
        }
    }

    async fn print_line(&mut self, text: &str) -> Result<(), ReplError> {
        let text = self.st.line(text);
        self.write(&text).await
    }

    /// End a half-written line (streamed text) before a prompt.
    async fn start_line(&mut self) -> Result<(), ReplError> {
        let end = self.st.break_line();
        if end.is_empty() {
            return Ok(());
        }
        self.write(end).await
    }

    fn dim(&self, s: &str) -> String {
        if self.opts.color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    async fn write(&mut self, text: &str) -> Result<(), ReplError> {
        self.out.write_all(text.as_bytes()).await?;
        self.out.flush().await?;
        Ok(())
    }
}

fn approve_cmd(request_id: &str, session_rule: Option<String>) -> Command {
    Command::Approve {
        request_id: request_id.to_string(),
        session_rule,
    }
}

fn deny_cmd(request_id: &str) -> Command {
    Command::Deny {
        request_id: request_id.to_string(),
        reason: None,
    }
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
fn ago(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}
