//! What the REPL prints for each event: a pure function of the event and a
//! [`RenderState`] (spec 7.11.8).
//!
//! - Assistant text streams as it arrives; reasoning is one `thinking…` line per step.
//! - Each tool call is one row: `<status> <tool> <target> <duration>[ · <note>]`.
//! - Diffs are coloured (green additions, red removals, dim context) when colour is on.
//! - After each turn: the receipt footer, then the evidence line (spec 7.11.7).
//!
//! Text from the model, tools, the repository or MCP servers is escaped first (spec 3.4):
//! [`printable`] for one-line fields, [`printable_multiline`] for streamed text and diffs.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::time::Instant;

use serde_json::Value;
use tautly_core::{
    DiffPreview, EffectView, ErrorKind, Event, FinishReason, MemoryChange, SaveKind, Savings,
    TestEvidence, UsageRole, printable, printable_multiline,
};

use crate::ReplOptions;

/// Columns a tool row's target may take.
pub const TARGET_COLUMNS: usize = 60;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// One turn's receipt, as the footer and `/cost` show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    pub turn: u32,
    pub steps: u32,
    pub tool_calls: u32,
    pub wall_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_hit_ratio: Option<f64>,
    pub cost_usd: Option<f64>,
    pub saved_tokens: u64,
    pub estimated: bool,
}

impl Receipt {
    pub fn from_event(e: &Event) -> Option<Receipt> {
        let Event::TurnReceipt {
            turn,
            tokens_in,
            tokens_out,
            cost_usd,
            wall_ms,
            cache_hit_ratio,
            steps,
            tool_calls,
            saved,
            estimated,
            ..
        } = e
        else {
            return None;
        };
        Some(Receipt {
            turn: *turn,
            steps: *steps,
            tool_calls: *tool_calls,
            wall_ms: *wall_ms,
            tokens_in: *tokens_in,
            tokens_out: *tokens_out,
            cache_hit_ratio: *cache_hit_ratio,
            cost_usd: *cost_usd,
            saved_tokens: saved_tokens(saved),
            estimated: *estimated,
        })
    }
}

/// The token estimates of a receipt's savings, summed.
pub fn saved_tokens(s: &Savings) -> u64 {
    s.index + s.diff_reads + s.context_gc + s.filters
}

/// A tool call between its proposal and its result.
struct Pending {
    target: String,
    started: Option<Instant>,
    /// Its preview diff was shown in an approval prompt.
    asked: bool,
}

/// What rendering remembers between events.
pub struct RenderState {
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
    /// The last thing printed ended a line (or nothing was printed yet).
    at_line_start: bool,
    /// This step's `thinking…` line was printed.
    thinking: bool,
    calls: HashMap<String, Pending>,
    /// Files the turn edited or deleted (for "no test run this turn").
    turn_edits: u32,
    /// The latest context use and budget.
    ctx: (u32, u32),
    /// Context warnings already shown.
    warned: HashSet<String>,
    session: Option<String>,
    /// The model the session is on, when known.
    pub model: Option<String>,
    /// The session's receipts, oldest first (with those of earlier runs of a resumed
    /// session, when the ledger had them).
    pub receipts: Vec<Receipt>,
}

impl Default for RenderState {
    fn default() -> Self {
        RenderState {
            clock: Box::new(Instant::now),
            at_line_start: true,
            thinking: false,
            calls: HashMap::new(),
            turn_edits: 0,
            ctx: (0, 0),
            warned: HashSet::new(),
            session: None,
            model: None,
            receipts: Vec::new(),
        }
    }
}

impl RenderState {
    /// Measure tool durations with `clock` instead of the system clock (tests).
    pub fn set_clock(&mut self, clock: impl Fn() -> Instant + Send + Sync + 'static) {
        self.clock = Box::new(clock);
    }

    /// Forget the session (before another one starts): its calls, receipts and context.
    pub fn new_session(&mut self) {
        let clock = std::mem::replace(&mut self.clock, Box::new(Instant::now));
        let model = self.model.take();
        *self = RenderState {
            clock,
            model,
            at_line_start: self.at_line_start,
            ..RenderState::default()
        };
    }

    /// `text` as the next output, after a line break if the last output ended mid-line.
    pub fn line(&mut self, text: impl AsRef<str>) -> String {
        let mut out = String::new();
        if !self.at_line_start {
            out.push('\n');
        }
        out.push_str(text.as_ref());
        if !out.ends_with('\n') {
            out.push('\n');
        }
        self.at_line_start = true;
        out
    }

    /// A newline when the last output ended mid-line (before a prompt), else nothing.
    pub fn break_line(&mut self) -> &'static str {
        if self.at_line_start {
            ""
        } else {
            self.at_line_start = true;
            "\n"
        }
    }

    /// Note that the user's typed line (and its newline) went to the terminal.
    pub fn typed(&mut self) {
        self.at_line_start = true;
    }

    /// The pending call `call_id` was shown in an approval prompt.
    pub fn mark_asked(&mut self, call_id: &str) {
        if let Some(p) = self.calls.get_mut(call_id) {
            p.asked = true;
        }
    }

    /// The target recorded for a call, when it was proposed.
    pub fn target(&self, call_id: &str) -> Option<&str> {
        self.calls.get(call_id).map(|p| p.target.as_str())
    }

    /// The session's cost so far: None when nothing is priced yet.
    fn session_cost(&self) -> Option<f64> {
        self.receipts
            .iter()
            .filter_map(|r| r.cost_usd)
            .reduce(|a, b| a + b)
    }
}

/// The text to print for `e` (None: nothing), updating `st`. Approval requests, memory
/// proposals and MCP server approvals render what the user decides on; the question
/// itself is the REPL's.
pub fn event(e: &Event, st: &mut RenderState, opts: &ReplOptions) -> Option<String> {
    let paint = |code: &str, s: &str| paint(opts.color, code, s);
    match e {
        Event::TurnStarted => {
            st.turn_edits = 0;
            st.thinking = false;
            None
        }
        Event::TextDelta { text } => {
            let text = printable_multiline(text);
            if text.is_empty() {
                return None;
            }
            st.at_line_start = text.ends_with('\n');
            Some(text)
        }
        Event::ReasoningDelta { .. } => {
            if st.thinking {
                return None;
            }
            st.thinking = true;
            Some(st.line(paint(DIM, "thinking…")))
        }
        Event::Usage { role, .. } => {
            // One response (one step) ended.
            if *role == UsageRole::Main {
                st.thinking = false;
            }
            None
        }
        Event::ToolCallProposed {
            call_id,
            tool,
            args,
        } => {
            st.calls.insert(
                call_id.clone(),
                Pending {
                    target: target(tool, args),
                    started: None,
                    asked: false,
                },
            );
            None
        }
        Event::ToolApprovalRequested {
            call_id,
            tool,
            args,
            reason,
            diff,
            ..
        } => {
            st.mark_asked(call_id);
            let mut out = format!(
                "{} {}  ({})\n{}",
                paint(YELLOW, "?"),
                paint(BOLD, &printable(tool)),
                printable(reason),
                approval_args(tool, args)
            );
            if let Some(d) = diff {
                out.push_str(&diff_text(&d.unified, opts.color));
            }
            Some(st.line(out))
        }
        Event::ToolStarted { call_id, .. } => {
            let now = (st.clock)();
            if let Some(p) = st.calls.get_mut(call_id) {
                p.started = Some(now);
            }
            None
        }
        Event::ToolOutputDelta { .. } => None,
        Event::ToolFinished {
            call_id,
            tool,
            output,
            is_error,
            blocked,
            effects,
            meta,
            diff,
        } => {
            let pending = st.calls.remove(call_id);
            let edits = effects
                .iter()
                .filter(|e| matches!(e, EffectView::Edited { .. } | EffectView::Deleted { .. }))
                .count();
            st.turn_edits += u32::try_from(edits).unwrap_or(u32::MAX);
            let status = if *blocked {
                "blocked"
            } else if !is_error {
                "ok"
            } else if output.starts_with("cancelled") {
                "cancelled"
            } else if output.starts_with("denied") {
                "denied"
            } else {
                "err"
            };
            let colour = match status {
                "ok" => GREEN,
                "err" => RED,
                "cancelled" => DIM,
                _ => YELLOW,
            };
            // No start time (a replay, or a call that never started): no duration.
            let secs = pending
                .as_ref()
                .and_then(|p| p.started)
                .map(|t| (st.clock)().saturating_duration_since(t).as_secs_f64());
            let shown = pending.as_ref().map_or("", |p| p.target.as_str());
            let mut row = format!("{} {}", paint(colour, status), printable(tool));
            if !shown.is_empty() {
                row.push(' ');
                row.push_str(shown);
            }
            if let Some(secs) = secs {
                let _ = write!(row, " {secs:.1}s");
            }
            let mut notes = Vec::new();
            if let Some(code) = meta.exit_code {
                notes.push(format!("exit {code}"));
            }
            if let Some((added, removed)) = diff.as_ref().map(diff_stat) {
                notes.push(format!("+{added} -{removed}"));
            }
            if let Some(saved) = meta.saved.filter(|s| s.bytes > 0) {
                let what = match saved.kind {
                    SaveKind::Filter => "filtered, ",
                    _ => "",
                };
                notes.push(format!(
                    "{what}saved ~{} tok (est.)",
                    tokens(u64::from(saved.bytes) / 4)
                ));
            }
            for n in notes {
                let _ = write!(row, " · {n}");
            }
            let mut out = st.line(row);
            // An applied diff the user did not already see in an approval.
            if let Some(d) = diff.as_ref().filter(|_| !pending.is_some_and(|p| p.asked)) {
                out.push_str(&diff_text(&d.unified, opts.color));
            }
            Some(out)
        }
        Event::ContextStatus {
            used,
            budget,
            warnings,
            ..
        } => {
            st.ctx = (*used, *budget);
            let new: Vec<String> = warnings
                .iter()
                .filter(|w| st.warned.insert((*w).clone()))
                .map(|w| format!("warning: {}", printable(w)))
                .collect();
            (!new.is_empty()).then(|| st.line(paint(YELLOW, &new.join("\n"))))
        }
        Event::Compacted {
            removed_turns,
            reason,
            ..
        } => Some(st.line(paint(
            DIM,
            &format!("compacted {removed_turns} turns ({})", wire_name(reason)),
        ))),
        Event::ContextCollected {
            elided_results,
            tokens_freed,
            ..
        } => Some(st.line(paint(
            DIM,
            &format!(
                "context: {elided_results} old results elided, ~{} tok freed",
                tokens(u64::from(*tokens_freed))
            ),
        ))),
        Event::MemoryProposed {
            change, replaces, ..
        } => Some(st.line(memory_text(change, replaces.len()))),
        Event::SessionChanged { session_id } => {
            let first = st.session.is_none();
            st.session = Some(session_id.clone());
            (!first).then(|| {
                st.line(paint(
                    DIM,
                    &format!("now on session {}", printable(session_id)),
                ))
            })
        }
        Event::TurnReceipt { .. } => {
            let r = Receipt::from_event(e)?;
            st.receipts.push(r.clone());
            if !opts.footer {
                return None;
            }
            let session = st.session_cost();
            let text = footer(&r, st.ctx, session);
            Some(st.line(paint(DIM, &text)))
        }
        Event::TurnFinished { reason, evidence } => {
            let mut lines = Vec::new();
            match reason {
                FinishReason::Completed => {}
                FinishReason::Cancelled => lines.push(paint(YELLOW, "turn cancelled")),
                FinishReason::Error => lines.push(paint(RED, "turn ended by an error")),
                other => lines.push(paint(
                    YELLOW,
                    &format!("turn ended: {}", wire_name(other).replace('_', " ")),
                )),
            }
            match evidence {
                Some(ev) => lines.push(paint(DIM, &evidence_line(ev))),
                None if st.turn_edits > 0 => lines.push(paint(DIM, "no test run this turn")),
                None => {}
            }
            st.turn_edits = 0;
            (!lines.is_empty()).then(|| st.line(lines.join("\n")))
        }
        Event::Error {
            kind,
            message,
            hint,
            ..
        } => {
            let mut text = format!("error: {}", printable(message));
            if *kind == ErrorKind::ProviderTransient {
                text.push_str(" (temporary)");
            }
            if let Some(h) = hint {
                let _ = write!(text, "\nhint: {}", printable(h));
            }
            Some(st.line(paint(RED, &text)))
        }
        Event::Warning { message } => {
            Some(st.line(paint(YELLOW, &format!("warning: {}", printable(message)))))
        }
        Event::SessionDiff {
            turn,
            files,
            unified,
            ..
        } => {
            let scope = turn.map_or_else(|| "the session".to_string(), |n| format!("turn {n}"));
            if files.is_empty() {
                return Some(st.line(format!("no changes by Tautly's tools in {scope}")));
            }
            let mut out = format!("changes in {scope}:\n");
            for f in files {
                let _ = writeln!(
                    out,
                    "  {} {} +{} -{}",
                    wire_name(&f.status),
                    printable(&f.path),
                    f.added,
                    f.removed
                );
            }
            out.push_str(&diff_text(unified, opts.color));
            Some(st.line(out))
        }
        Event::ModelChanged { model, provider } => {
            st.model = Some(model.as_str().to_string());
            Some(st.line(format!(
                "model: {} ({})",
                printable(model.as_str()),
                printable(provider)
            )))
        }
        Event::McpServerApprovalRequested {
            server,
            source,
            launch,
            ..
        } => {
            // The engine escaped `server` and `launch` already.
            Some(st.line(format!(
                "{} MCP server `{server}` ({}) wants to start:\n  {launch}",
                paint(YELLOW, "?"),
                printable(source)
            )))
        }
        Event::Undone {
            turn,
            restored,
            deleted,
            dry_run,
            backup_ref,
            note,
        } => {
            let list = |v: &[String]| {
                if v.is_empty() {
                    "nothing".to_string()
                } else {
                    v.iter()
                        .map(|p| printable(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            };
            let mut out = if *dry_run {
                format!(
                    "undo turn {turn} would restore: {}\n  and delete: {}",
                    list(restored),
                    list(deleted)
                )
            } else {
                format!(
                    "undid turn {turn}: restored {}; deleted {}",
                    list(restored),
                    list(deleted)
                )
            };
            if let Some(b) = backup_ref {
                let _ = write!(out, "\n  snapshot before: {}", printable(b));
            }
            if let Some(n) = note {
                let _ = write!(out, "\n  note: {}", printable(n));
            }
            Some(st.line(out))
        }
        Event::Rewound { at, report, .. } => {
            Some(st.line(materialized(&format!("rewind to step {at}"), report)))
        }
        Event::Switched { branch, report } => {
            let short: String = branch.chars().take(8).collect();
            Some(st.line(materialized(
                &format!("switch to {}", printable(&short)),
                report,
            )))
        }
        Event::McpServerStatus {
            server,
            state,
            tools,
            message,
        } => {
            let mut text = format!(
                "mcp {}: {} ({tools} tools)",
                printable(server),
                wire_name(state).replace('_', " ")
            );
            if let Some(m) = message {
                let _ = write!(text, " · {}", printable(m));
            }
            Some(st.line(paint(DIM, &text)))
        }
        Event::Replayed { event: logged, .. } => match &**logged {
            // A logged switch is history: the session's model stays what it is.
            Event::ModelChanged { model, provider } => Some(st.line(format!(
                "model: {} ({})",
                printable(model.as_str()),
                printable(provider)
            ))),
            // Nothing replayed is answered: shown, never asked.
            Event::Replayed { .. } => None,
            inner => event(inner, st, opts),
        },
        Event::UserMessage { text } => {
            let text = printable_multiline(text);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            Some(st.line(paint(BOLD, &format!("> {}", text.replace('\n', "\n  ")))))
        }
        Event::ReplayFinished {
            events, cancelled, ..
        } => Some(st.line(paint(
            DIM,
            &if *cancelled {
                format!("replay cancelled after {events} events")
            } else {
                format!("replay finished: {events} events")
            },
        ))),
        _ => None,
    }
}

/// What a rewind or switch (`what`) did or would do: what it restores and deletes, the
/// conflicts, the backup and the note. Paths are escaped.
fn materialized(what: &str, r: &tautly_core::MaterializeReport) -> String {
    let list = |v: &[String]| {
        if v.is_empty() {
            "nothing".to_string()
        } else {
            v.iter()
                .map(|p| printable(p))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut out = if r.applied {
        format!(
            "{what}: restored {}; deleted {}",
            list(&r.restored),
            list(&r.deleted)
        )
    } else if r.dry_run {
        format!(
            "{what} would restore: {}\n  and delete: {}",
            list(&r.restored),
            list(&r.deleted)
        )
    } else {
        format!("{what} not done: nothing was changed")
    };
    if !r.conflicts.is_empty() {
        let _ = write!(
            out,
            "\n  conflicts (changed since the agent left them): {}",
            list(&r.conflicts)
        );
    }
    if !r.blocked.is_empty() {
        let _ = write!(
            out,
            "\n  blocked (a directory, link or file is in the way; never written): {}",
            list(&r.blocked)
        );
    }
    if let Some(b) = &r.backup_ref {
        let _ = write!(out, "\n  snapshot before: {}", printable(b));
    } else if r.dry_run && !(r.restored.is_empty() && r.deleted.is_empty()) {
        let _ = match r.backup.as_deref() {
            Some("log") => write!(out, "\n  backup: kept in the session log"),
            Some(_) => write!(out, "\n  backup: a git snapshot"),
            None => write!(out, "\n  backup: none (what it overwrites is not kept)"),
        };
    }
    if let Some(n) = &r.note {
        let _ = write!(out, "\n  note: {}", printable(n));
    }
    out
}

/// The receipt footer: `turn N · S steps · 12.4s · ctx <used>/<budget> tok · cache 86% ·
/// $0.0712 (session $0.4120) · saved ~41k tok (est.)`. Usage the engine had to estimate
/// is flagged at the end.
pub fn footer(r: &Receipt, ctx: (u32, u32), session_usd: Option<f64>) -> String {
    let mut s = format!(
        "turn {} · {} steps · {:.1}s · ctx {}/{} tok · cache {}% · {} (session {}) · saved ~{} tok (est.)",
        r.turn,
        r.steps,
        r.wall_ms as f64 / 1000.0,
        ctx.0,
        ctx.1,
        percent(r.cache_hit_ratio),
        usd(r.cost_usd),
        usd(session_usd),
        tokens(r.saved_tokens),
    );
    if r.estimated {
        s.push_str(" · usage estimated");
    }
    s
}

/// `/cost`: the last turn's receipt and the session's.
pub fn cost(st: &RenderState) -> String {
    let Some(last) = st.receipts.last() else {
        return "no turn has finished in this session yet\n".to_string();
    };
    let est = |e: bool| if e { " · usage estimated" } else { "" };
    let mut out = format!(
        "last turn: turn {} · {} steps · {} tool calls · {:.1}s · {} in / {} out tok · cache {}% · {} · saved ~{} tok (est.){}\n",
        last.turn,
        last.steps,
        last.tool_calls,
        last.wall_ms as f64 / 1000.0,
        tokens(last.tokens_in),
        tokens(last.tokens_out),
        percent(last.cache_hit_ratio),
        usd(last.cost_usd),
        tokens(last.saved_tokens),
        est(last.estimated),
    );
    let sum = |f: fn(&Receipt) -> u64| st.receipts.iter().map(f).sum::<u64>();
    let _ = writeln!(
        out,
        "session: {} turns · {} steps · {} tool calls · {:.1}s · {} in / {} out tok · {} · saved ~{} tok (est.){}",
        st.receipts.len(),
        sum(|r| u64::from(r.steps)),
        sum(|r| u64::from(r.tool_calls)),
        sum(|r| r.wall_ms) as f64 / 1000.0,
        tokens(sum(|r| r.tokens_in)),
        tokens(sum(|r| r.tokens_out)),
        usd(st.session_cost()),
        tokens(sum(|r| r.saved_tokens)),
        est(st.receipts.iter().any(|r| r.estimated)),
    );
    out
}

/// `last test: <command> · exit <code> · fresh|stale (N edits since)`.
pub fn evidence_line(ev: &TestEvidence) -> String {
    let exit = ev
        .exit_code
        .map_or_else(|| "killed".to_string(), |c| format!("exit {c}"));
    let freshness = if !ev.stale {
        "fresh".to_string()
    } else {
        let plural =
            |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut since = Vec::new();
        if ev.edits_since > 0 {
            since.push(plural(ev.edits_since, "edit", "edits"));
        }
        if ev.execs_since > 0 {
            since.push(plural(ev.execs_since, "command", "commands"));
        }
        if since.is_empty() {
            "stale".to_string()
        } else {
            format!("stale ({} since)", since.join(", "))
        }
    };
    format!(
        "last test: {} · {exit} · {freshness}",
        printable(&ev.command)
    )
}

/// Most lines of JSON arguments (MCP and other tools) an approval shows before
/// `(N more lines)`. A command is never cut.
pub const APPROVAL_LINES: usize = 200;

/// What an approval shows of a call, in full (never the row's 60-column cut), escaped
/// (spec 3.4):
/// - a command, every line of it, however long (a newline starts a new `>` line, so two
///   commands never read as one; a final newline adds no line);
/// - a path, or a URL;
/// - otherwise (MCP tools and the rest) the arguments as pretty-printed JSON, capped at
///   [`APPROVAL_LINES`] lines with a marker.
pub fn approval_args(tool: &str, args: &Value) -> String {
    let arg = |k: &str| args.get(k).and_then(Value::as_str);
    let split = |text: &str| -> Vec<String> {
        let text = printable_multiline(text);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        text.split('\n').map(str::to_string).collect()
    };
    let (lines, cap): (Vec<String>, usize) = match (tool, arg("command"), arg("path"), arg("url")) {
        ("bash", Some(cmd), _, _) => (
            split(cmd)
                .into_iter()
                .enumerate()
                .map(|(i, l)| format!("  {} {l}", if i == 0 { "$" } else { ">" }))
                .collect(),
            usize::MAX,
        ),
        ("edit" | "write", _, Some(path), _) => {
            (vec![format!("  {}", printable(path))], usize::MAX)
        }
        ("web_fetch", _, _, Some(url)) => (vec![format!("  {}", printable(url))], usize::MAX),
        _ => {
            let json = serde_json::to_string_pretty(args).unwrap_or_default();
            (
                split(&json).into_iter().map(|l| format!("  {l}")).collect(),
                APPROVAL_LINES,
            )
        }
    };
    let mut out = String::new();
    for l in lines.iter().take(cap) {
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > cap {
        let _ = writeln!(out, "  ({} more lines)", lines.len() - cap);
    }
    out
}

/// A call's target as its row shows it: the command, path, pattern or URL, escaped and
/// cut to [`TARGET_COLUMNS`].
pub fn target(tool: &str, args: &Value) -> String {
    let arg = |k: &str| args.get(k).and_then(Value::as_str);
    let raw = match tool {
        "bash" => arg("command").map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")),
        "web_fetch" => arg("url").map(str::to_string),
        _ => ["path", "pattern", "query", "name", "url"]
            .iter()
            .find_map(|k| arg(k))
            .map(str::to_string)
            .or_else(|| arg("patch").map(patch_paths)),
    }
    .unwrap_or_default();
    cut(&printable(&raw), TARGET_COLUMNS)
}

/// The files a patch names (`*** Update File: <path>` and the like).
fn patch_paths(patch: &str) -> String {
    let paths: Vec<&str> = patch
        .lines()
        .filter_map(|l| {
            [
                "*** Update File: ",
                "*** Add File: ",
                "*** Delete File: ",
                "+++ b/",
            ]
            .iter()
            .find_map(|p| l.strip_prefix(p))
        })
        .map(str::trim)
        .collect();
    paths.join(", ")
}

fn cut(s: &str, columns: usize) -> String {
    if s.chars().count() <= columns {
        return s.to_string();
    }
    let mut out: String = s.chars().take(columns - 1).collect();
    out.push('…');
    out
}

/// `+added -removed` lines of a unified diff (headers not counted).
fn diff_stat(d: &DiffPreview) -> (u32, u32) {
    let mut stat = (0, 0);
    for l in d.unified.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            stat.0 += 1;
        } else if l.starts_with('-') {
            stat.1 += 1;
        }
    }
    stat
}

/// A unified diff, escaped line by line, coloured when `color` is on.
pub fn diff_text(unified: &str, color: bool) -> String {
    let mut out = String::new();
    // Split on `\n` only: a `\r` stays and shows (`\r`), so a CRLF change is visible.
    for raw in unified.split_terminator('\n') {
        let line = printable_multiline(raw);
        let code = if raw.starts_with("+++") || raw.starts_with("---") {
            BOLD
        } else if raw.starts_with('+') {
            GREEN
        } else if raw.starts_with('-') {
            RED
        } else if raw.starts_with("@@") {
            YELLOW
        } else {
            DIM
        };
        out.push_str(&paint(color, code, &line));
        out.push('\n');
    }
    out
}

fn memory_text(change: &MemoryChange, replaces: usize) -> String {
    let mut out = String::from("memory proposal:\n");
    if let Some(e) = &change.upsert {
        let verb = if replaces > 0 { "update" } else { "save" };
        let kind = serde_json::to_value(e.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let _ = writeln!(out, "  {verb} [{kind}] {}", printable(&e.title));
        for l in printable_multiline(&e.body).lines() {
            let _ = writeln!(out, "    {l}");
        }
    }
    if !change.delete.is_empty() {
        let ids: Vec<String> = change.delete.iter().map(|i| printable(i)).collect();
        let _ = writeln!(out, "  delete {}", ids.join(", "));
    }
    out
}

fn paint(color: bool, code: &str, s: &str) -> String {
    if color && !s.is_empty() {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

/// A protocol enum's wire name (`max_turns`).
fn wire_name(v: &impl serde::Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn percent(ratio: Option<f64>) -> u32 {
    ratio.map_or(0, |r| (r * 100.0).round().clamp(0.0, 100.0) as u32)
}

fn usd(v: Option<f64>) -> String {
    v.map_or_else(|| "$?".to_string(), |v| format!("${v:.4}"))
}

/// `950`, `1.2k`, `41k`, `1.3M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1000.0),
        10_000..1_000_000 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}
