//! Context garbage collection (spec 7.2): a tool result that a later result makes redundant
//! is replaced by `[elided: superseded at turn N]`, keeping the tool-call id and so the
//! `tool_use`/`tool_result` pairing. Only the history changes, never the stable prefix, the
//! pinned items or the working state.
//!
//! The engine records every result with [`Context::push_tool_result`] and, at turn start
//! only, asks the scheduler ([`Context::gc_decision`]) and then [`Context::collect`]s. A
//! result is superseded when
//!
//! - it is a read and every target it showed is covered by a later read of the same path
//!   that shows at least those lines in full (or the same lines again), or by a later write
//!   of the path. A read that answered `unchanged`/a diff never covers its base, but is
//!   itself covered in the same ways;
//! - it is a `bash` result and the same command string ran again later;
//! - it is a failed `edit` and every path it named was edited successfully later.
//!
//! The newest result of a key is never elided.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use hx_provider::{Content, Price};
use hx_tools::{AppliedEffect, LineRange, Shown};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schedule::{self, MutationReason};
use crate::{Context, Turn};

/// Start of every stub written by [`Context::collect`].
const STUB_PREFIX: &str = "[elided: superseded at turn ";

fn stub(turn: u32) -> String {
    format!("{STUB_PREFIX}{turn}]")
}

/// What [`Context::collect`] removed, enough to undo it with [`Context::restore_gc`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcRecord {
    /// Tool-call id and original output of each elided result, oldest first.
    pub elided: Vec<(String, String)>,
    pub tokens_freed: u32,
    /// Why the scheduler allowed the collection.
    pub reason: MutationReason,
    /// Every path any elided read result covered, in first-seen order. The engine passes
    /// each to `ReadTracker::forget_path`: a re-read must not answer `unchanged since turn
    /// N` or a diff against a result the model can no longer see.
    pub forgotten_paths: Vec<PathBuf>,
}

/// One target of a read result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadTarget {
    pub path: PathBuf,
    pub range: LineRange,
    pub shown: Shown,
}

/// What [`Context::push_tool_result`] remembers about a tool result: the keys the collector
/// matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultMeta {
    pub call_id: String,
    pub tool: String,
    /// The session turn the result belongs to, as the engine numbered it.
    pub turn: u32,
    /// Whether [`Context::collect`] has replaced the output with a stub.
    pub elided: bool,
    /// The read targets, from the result's `Read` effects.
    pub reads: Vec<ReadTarget>,
    /// Paths the result wrote, from its `Edited` effects.
    pub writes: Vec<PathBuf>,
    /// For a failed `edit`: the paths the call named, made absolute against the root when it
    /// is known.
    pub failed_edit: Vec<PathBuf>,
    /// For `bash`: the command string.
    pub command: Option<String>,
}

/// A result to elide: its record, where its output sits in the history, and the turn of the
/// result that supersedes it.
struct Planned {
    meta: usize,
    at: Pos,
    by_turn: u32,
}

/// `(turn, message, content)` indices of a `ToolResult` in the history.
type Pos = (usize, usize, usize);

impl Context {
    /// Record a tool result for the collector. Call it for every result, in the order the
    /// results were produced; the engine appends the result to the history itself, before or
    /// after. `effects` are the result's applied effects: reads and writes are keyed by the
    /// paths in them, never by what the model wrote in `args`, which is only used for the
    /// command of a `bash` call and for the paths of an `edit` that failed (no effects).
    pub fn push_tool_result(
        &mut self,
        call_id: &str,
        tool: &str,
        args: &Value,
        effects: &[AppliedEffect],
        is_error: bool,
        turn: u32,
    ) {
        let mut reads = Vec::new();
        let mut writes = Vec::new();
        for e in effects {
            match e {
                AppliedEffect::Read {
                    path, range, shown, ..
                } => reads.push(ReadTarget {
                    path: path.clone(),
                    range: *range,
                    shown: *shown,
                }),
                AppliedEffect::Edited { path, .. } => writes.push(path.clone()),
                AppliedEffect::Deleted { .. } | AppliedEffect::Exec { .. } => {}
            }
        }
        let failed_edit = if is_error && matches!(tool, "edit" | "patch") {
            edit_paths(args)
                .into_iter()
                .map(|p| absolute(self.root.as_deref(), &p))
                .collect()
        } else {
            Vec::new()
        };
        let command = (tool == "bash")
            .then(|| args.get("command").and_then(Value::as_str))
            .flatten()
            .map(str::to_string);
        self.results.push(ResultMeta {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            turn,
            elided: false,
            reads,
            writes,
            failed_edit,
            command,
        });
    }

    /// Estimated tokens [`collect`](Self::collect) would free now.
    pub fn pending_gc(&self) -> u32 {
        self.plan()
            .iter()
            .map(|p| self.freed_by(p))
            .fold(0, u32::saturating_add)
    }

    /// Whether to collect now, and why: [`schedule::decide`] on what is pending, `None` when
    /// nothing is. `used` is the current context size in tokens and `window` the model's
    /// window, `ttl` and `price` the model's cache lifetime and catalog price. Pass the
    /// result to [`collect`](Self::collect).
    pub fn gc_decision(
        &self,
        now: Instant,
        ttl: Option<Duration>,
        price: Option<&Price>,
        used: u32,
        window: u32,
    ) -> Option<MutationReason> {
        let pending = self.pending_gc();
        if pending == 0 {
            return None;
        }
        schedule::decide(&self.clock, now, ttl, pending, price, used, window)
    }

    /// Elide every superseded result, all at once (one cache epoch). Does nothing without a
    /// `reason`, or when nothing is superseded. A pure function of the context's state: when
    /// to call it (turn start, after [`gc_decision`](Self::gc_decision)) is the engine's call.
    pub fn collect(&mut self, reason: Option<MutationReason>) -> Option<GcRecord> {
        let reason = reason?;
        let plan = self.plan();
        if plan.is_empty() {
            return None;
        }
        let mut elided = Vec::with_capacity(plan.len());
        let mut tokens_freed = 0u32;
        let mut forgotten_paths: Vec<PathBuf> = Vec::new();
        for p in &plan {
            tokens_freed = tokens_freed.saturating_add(self.freed_by(p));
            let (t, m, c) = p.at;
            let Content::ToolResult { output, .. } = &mut self.history[t].messages[m].content[c]
            else {
                continue;
            };
            let original = std::mem::replace(output, stub(p.by_turn));
            let meta = &mut self.results[p.meta];
            meta.elided = true;
            for target in &meta.reads {
                if !forgotten_paths.contains(&target.path) {
                    forgotten_paths.push(target.path.clone());
                }
            }
            elided.push((meta.call_id.clone(), original));
        }
        Some(GcRecord {
            elided,
            tokens_freed,
            reason,
            forgotten_paths,
        })
    }

    /// Undo [`collect`](Self::collect): put the original outputs back, byte for byte. A
    /// result that is no longer in the history (compacted away; restore it with
    /// `uncompact` first) or no longer a stub is left alone.
    pub fn restore_gc(&mut self, rec: &GcRecord) {
        let found: Vec<(Pos, &String, &String)> = {
            let at = result_positions(&self.history);
            rec.elided
                .iter()
                .filter_map(|(id, original)| at.get(id.as_str()).map(|p| (*p, id, original)))
                .collect()
        };
        for ((t, m, c), id, original) in found {
            let Content::ToolResult { output, .. } = &mut self.history[t].messages[m].content[c]
            else {
                continue;
            };
            if !output.starts_with(STUB_PREFIX) {
                continue;
            }
            output.clone_from(original);
            for meta in self.results.iter_mut().filter(|r| &r.call_id == id) {
                meta.elided = false;
            }
        }
    }

    /// Estimated tokens eliding `p` frees.
    fn freed_by(&self, p: &Planned) -> u32 {
        let (t, m, c) = p.at;
        let Content::ToolResult { output, .. } = &self.history[t].messages[m].content[c] else {
            return 0;
        };
        self.tokens
            .estimate(output)
            .saturating_sub(self.tokens.estimate(&stub(p.by_turn)))
    }

    /// The superseded results still in the history and not yet elided, oldest first.
    ///
    /// Walks the records newest to oldest, keeping what the later ones cover, so each
    /// record sees the nearest later result that supersedes it. Elided records keep
    /// covering: whatever superseded them covers everything they did (the relation is
    /// transitive), and their stubs are why a path's baselines were forgotten.
    fn plan(&self) -> Vec<Planned> {
        let at = result_positions(&self.history);
        let mut later_cmd: HashMap<&str, Later> = HashMap::new();
        let mut later_write: HashMap<&Path, Later> = HashMap::new();
        // Newest first: the nearest covering read is the last pushed.
        let mut later_writes: Vec<(&Path, Later)> = Vec::new();
        let mut later_reads: HashMap<&Path, Vec<(LineRange, bool, Later)>> = HashMap::new();

        let mut plan = Vec::new();
        for (i, meta) in self.results.iter().enumerate().rev() {
            let Some(&pos) = at.get(meta.call_id.as_str()) else {
                continue;
            };
            let me = Later {
                idx: i,
                turn: meta.turn,
            };
            if !meta.elided {
                let by = if let Some(cmd) = &meta.command {
                    later_cmd.get(cmd.as_str()).map(|l| l.turn)
                } else if !meta.reads.is_empty() {
                    meta.reads
                        .iter()
                        .map(|t| {
                            let by_write = later_write.get(t.path.as_path()).copied();
                            let by_read = later_reads
                                .get(t.path.as_path())
                                .and_then(|v| {
                                    v.iter().rev().find(|(r, full, _)| {
                                        covers(*r, *full, t.range, t.shown == Shown::Partial)
                                    })
                                })
                                .map(|(_, _, l)| *l);
                            nearest(by_write, by_read).map(|l| l.turn)
                        })
                        .try_fold(0, |max: u32, turn| turn.map(|t| max.max(t)))
                } else if !meta.failed_edit.is_empty() {
                    meta.failed_edit
                        .iter()
                        .map(|f| {
                            later_writes
                                .iter()
                                .rev()
                                .find(|(w, _)| same_file(f, w))
                                .map(|(_, l)| l.turn)
                        })
                        .try_fold(0, |max: u32, turn| turn.map(|t| max.max(t)))
                } else {
                    None
                };
                if let Some(by_turn) = by {
                    plan.push(Planned {
                        meta: i,
                        at: pos,
                        by_turn,
                    });
                }
            }
            if let Some(cmd) = &meta.command {
                later_cmd.insert(cmd, me);
            }
            for w in &meta.writes {
                later_write.insert(w, me);
                later_writes.push((w, me));
            }
            for t in &meta.reads {
                // `unchanged`/diff answers lean on their base and cover nothing.
                if matches!(t.shown, Shown::Full | Shown::Partial) {
                    later_reads.entry(&t.path).or_default().push((
                        t.range,
                        t.shown == Shown::Full,
                        me,
                    ));
                }
            }
        }
        plan.reverse();
        plan
    }
}

/// A result later than the one being examined: its record index and turn.
#[derive(Debug, Clone, Copy)]
struct Later {
    idx: usize,
    turn: u32,
}

/// The nearer (lower index) of two optional superseders.
fn nearest(a: Option<Later>, b: Option<Later>) -> Option<Later> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.idx <= b.idx { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Whether a later read of `later` lines (`full`: shown in full, else cut short) replaces an
/// earlier read of `earlier` (`earlier_cut`: it was cut short): a full read shows all of
/// the earlier lines or more, and a cut-short one replaces only another cut-short read of
/// the same lines, never a complete one.
fn covers(later: LineRange, full: bool, earlier: LineRange, earlier_cut: bool) -> bool {
    if later == earlier {
        return full || earlier_cut;
    }
    full && later.start.unwrap_or(1) <= earlier.start.unwrap_or(1)
        && later.end.unwrap_or(usize::MAX) >= earlier.end.unwrap_or(usize::MAX)
}

/// Where each tool result sits in the history, by call id.
fn result_positions(history: &[Turn]) -> HashMap<&str, Pos> {
    let mut at = HashMap::new();
    for (t, turn) in history.iter().enumerate() {
        for (m, message) in turn.messages.iter().enumerate() {
            for (c, content) in message.content.iter().enumerate() {
                if let Content::ToolResult { id, .. } = content {
                    at.insert(id.as_str(), (t, m, c));
                }
            }
        }
    }
    at
}

/// The paths an `edit` call names: its `path`, or the files of its patch.
fn edit_paths(args: &Value) -> Vec<PathBuf> {
    if let Some(p) = args.get("path").and_then(Value::as_str) {
        vec![PathBuf::from(p)]
    } else if let Some(patch) = args.get("patch").and_then(Value::as_str) {
        hx_tools::edit::patch::touched(patch)
            .map(|t| t.into_iter().map(|t| t.path).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// `path` joined to `root` when relative and `root` is known, without `.` and `..`.
fn absolute(root: Option<&Path>, path: &Path) -> PathBuf {
    let joined = match root {
        Some(root) if path.is_relative() => root.join(path),
        _ => path.to_path_buf(),
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if out.file_name().is_some() => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether `named` (a failed call's path, absolute when the root was known) is the written
/// file `written`. A relative path that could not be resolved matches nothing: guessing by
/// its tail could elide the failure of another file.
fn same_file(named: &Path, written: &Path) -> bool {
    named == written
}
