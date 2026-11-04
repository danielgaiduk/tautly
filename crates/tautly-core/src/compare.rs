//! Compare (spec 7.12.6): sessions side by side, from their logs alone (`tautly compare`).
//! Nothing here calls a provider or a tool, or reads a file outside the logs, their blob
//! stores and the cost ledger.
//!
//! - **Fork point.** [`fork_point`] walks each session's parent chain (`Header.parent`:
//!   the parent and the offset of the last record the branch copied) to the nearest
//!   session both chains hold, and takes the earlier of the two offsets they left it at,
//!   as a step ([`step_at_offset`]). A parent whose log was deleted still has an id and an
//!   offset in its branch's header, and its branch's log holds the copied records.
//! - **Figures** ([`summarize`]) count the records after the fork point: the branch's own
//!   `Forked` boundary when its header names that step, else the records from the first
//!   step past it (as a rewind to that step would cut).
//!   - Wall time is `Σ(TurnEnd.t − TurnStart.t)` over the turns that start after the cut
//!     (a `SendMessage` is logged when it arrives, which may be while another turn runs).
//!   - Steps are distinct `Request.step` values: a retry repeats its step.
//!   - Cost: the cost ledger's rows of the session (discarded and estimated attempts
//!     included) when the counted range is exactly the session's own records; otherwise,
//!     or without ledger rows, the logged usage: each complete response's last usage and
//!     its discarded attempts at its step's model, and every `Charged` record at its own
//!     model, priced with the catalog (the receipt's and resume's rule). Estimates and
//!     unpriced models are marked.
//!   - Files and the diff stat come from the `Edit` and `Undone` records after the cut:
//!     each path's earliest content there against the last one a tool left, from the
//!     log's blob store. Command effects are not in them.
//! - **Untrusted logs.** A path that is absolute, has a `..` or `.` component or a NUL
//!   byte is left out (with one note), resolved under the session's own root only; ids
//!   from headers must be plain session ids before they name a file; records of an
//!   unknown type make one note. Callers escape every string before printing it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tautly_provider::{Catalog, ModelId, ProviderEvent, RawUsage, cost_usd};
use tautly_session::{BlobRef, Header, SessionReader};
use tautly_tools::PersistedEffect;
use tautly_tools::preview::{FileChange, diff_files};

use crate::checkpoint::check_session;
use crate::evidence::TestEvidence;
use crate::ledger::Ledger;
use crate::record::SessionRecord;
use crate::timetravel::{StepCounter, Writes, befores, step_at_offset};

/// The most sessions one comparison takes.
pub const MAX_SESSIONS: usize = 5;
/// The longest parent chain followed.
const MAX_DEPTH: usize = 64;
/// The tool whose failures count as edit failures.
const EDIT_TOOL: &str = "edit";

/// One session's figures, counted after its fork point (or over the whole session).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchSummary {
    pub session_id: String,
    /// The last `Labeled` record's label, else the header's.
    pub label: Option<String>,
    /// Every model a request or charge was made on, in order of first use.
    pub models: Vec<ModelId>,
    pub user_turns: u32,
    /// Distinct provider steps.
    pub steps: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    /// USD of the priced usage (none: nothing was priced).
    pub cost_usd: Option<f64>,
    /// Some of the usage is the engine's estimate.
    pub cost_estimated: bool,
    /// Some usage was on a model without a known price: `cost_usd` covers the rest.
    pub cost_partial: bool,
    /// `Σ(TurnEnd.t − TurnStart.t)`.
    pub wall_ms: u64,
    /// Tool calls that ran.
    pub tool_calls: u32,
    /// Calls the loop breaker refused.
    pub blocked: u32,
    /// `edit` calls that failed.
    pub edit_failures: u32,
    /// The last test run (stale when tools changed files after its turn).
    pub evidence: Option<TestEvidence>,
    /// Root-relative paths the session's tools changed.
    pub files: BTreeSet<String>,
    /// Lines added and removed, net per file.
    pub added: u32,
    pub removed: u32,
    /// What the figures could not count, for the reader.
    pub notes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    #[error("cannot read the session log: {0}")]
    Log(#[from] tautly_session::Error),
}

// ------------------------------------------------------------------- fork point

/// One session of a parent chain.
struct Link {
    id: String,
    /// The offset of the last of its records the session before it in the chain copied
    /// (none: the chain's own session).
    through: Option<u64>,
    /// Its log, when it exists and its header names this id.
    log: Option<PathBuf>,
}

/// `<data_home>/sessions/<project>/<id>.jsonl`, when both are plain ids.
fn log_path(data_home: &Path, project: &str, id: &str) -> Option<PathBuf> {
    (check_session(project).is_ok() && check_session(id).is_ok()).then(|| {
        data_home
            .join("sessions")
            .join(project)
            .join(format!("{id}.jsonl"))
    })
}

/// The header of the log at `path`, when it is session `id`'s.
fn header_of(path: &Path, id: &str) -> Option<Header> {
    let reader = SessionReader::<serde::de::IgnoredAny>::open(path).ok()?;
    let h = reader.header();
    (h.session_id == id).then(|| h.clone())
}

/// `h`'s session, then its parent, its parent's parent, ... while their logs say.
fn chain(h: &Header, data_home: &Path) -> Vec<Link> {
    let own = log_path(data_home, &h.project, &h.session_id)
        .filter(|p| header_of(p, &h.session_id).is_some());
    let mut out = vec![Link {
        id: h.session_id.clone(),
        through: None,
        log: own,
    }];
    let mut seen = HashSet::from([h.session_id.clone()]);
    let mut next = h.parent.clone().map(|p| (p, h.project.clone()));
    while let Some(((id, offset), project)) = next.take() {
        if out.len() > MAX_DEPTH || check_session(&id).is_err() || !seen.insert(id.clone()) {
            break;
        }
        let path = log_path(data_home, &project, &id);
        let header = path.as_deref().and_then(|p| header_of(p, &id));
        next = header
            .as_ref()
            .and_then(|h| h.parent.clone().map(|p| (p, h.project.clone())));
        out.push(Link {
            id,
            through: Some(offset),
            log: header.and(path),
        });
    }
    out
}

/// Where sessions `a` and `b` branched apart: the nearest session both descend from (or
/// one of them, when the other descends from it) and the step at the earlier of the two
/// points they left it. None when they share no ancestor, or are the same session.
/// Parent logs are looked up under `data_home`. Blocking I/O (the logs).
pub fn fork_point(a: &Header, b: &Header, data_home: &Path) -> Option<(String, u32)> {
    let (ca, cb) = (chain(a, data_home), chain(b, data_home));
    for (i, la) in ca.iter().enumerate() {
        let Some(j) = cb.iter().position(|lb| lb.id == la.id) else {
            continue;
        };
        let lb = &cb[j];
        // The earlier cut, and the branch log that copied the session through it.
        let (cut, copy) = match (la.through, lb.through) {
            (None, None) => return None,
            (Some(x), None) => (x, &ca[i - 1]),
            (None, Some(y)) => (y, &cb[j - 1]),
            (Some(x), Some(y)) if x <= y => (x, &ca[i - 1]),
            (Some(_), Some(y)) => (y, &cb[j - 1]),
        };
        let log = la.log.as_ref().or(copy.log.as_ref())?;
        let reader = SessionReader::<SessionRecord>::open(log).ok()?;
        return Some((la.id.clone(), step_at_offset(&reader, cut)));
    }
    None
}

// ------------------------------------------------------------------- figures

/// Which records count.
#[derive(Clone, Copy)]
enum Cut {
    /// All of them.
    Whole,
    /// Those after this offset (the session's own fork boundary).
    Offset(u64),
    /// Those from the first step past this one.
    Step(u32),
}

/// The figures of `reader`'s session after step `from_step` of its lineage (none: the
/// whole session), with cost from `ledger` when it covers exactly that range, else from
/// the logged usage priced with `catalog` (`Config::effective_catalog`). Blocking I/O
/// (the log, its blobs, the ledger).
pub fn summarize(
    reader: &SessionReader<SessionRecord>,
    ledger: Option<&Ledger>,
    catalog: &Catalog,
    from_step: Option<u32>,
) -> Result<BranchSummary, CompareError> {
    let header = reader.header();
    let cut = match (from_step, &header.parent) {
        (None, _) => Cut::Whole,
        (Some(n), Some((_, off))) if step_at_offset(reader, *off) == n => Cut::Offset(*off),
        (Some(n), _) => Cut::Step(n),
    };
    // The ledger holds a session's own requests: after its fork, or all of a root's.
    let own_range = match cut {
        Cut::Whole => header.parent.is_none(),
        Cut::Offset(_) => true,
        Cut::Step(_) => false,
    };
    let mut t = Tally::new(header);
    let mut steps = StepCounter::default();
    let mut counting = matches!(cut, Cut::Whole);
    for entry in reader.entries() {
        let e = entry?;
        if let SessionRecord::Labeled { label } = &e.rec {
            t.label = Some(label.clone());
        }
        steps.see(&e.rec);
        if !counting {
            counting = match cut {
                Cut::Whole => true,
                Cut::Offset(off) => e.offset > off,
                Cut::Step(n) => {
                    matches!(
                        e.rec,
                        SessionRecord::TurnStart { .. } | SessionRecord::Request { .. }
                    ) && steps.last > n
                }
            };
        }
        // Edits before the cut leave the kinds behind them; the next result clears them.
        let wrote = t.writes.see(&e.rec);
        if counting {
            t.see(e.t_ms, &e.rec, wrote);
        }
    }
    t.close_response();
    Ok(t.finish(reader, ledger, catalog, own_range))
}

/// The counts of one pass over a log.
struct Tally {
    session_id: String,
    label: Option<String>,
    root: PathBuf,
    models: Vec<ModelId>,
    user_turns: u32,
    steps: BTreeSet<u32>,
    turn_start: Option<u64>,
    wall_ms: u64,
    /// The open step's model and its response's latest (cumulative) usage.
    model: Option<ModelId>,
    response: Option<RawUsage>,
    usage: Vec<(Option<ModelId>, RawUsage)>,
    estimated: bool,
    calls: HashMap<String, String>,
    tool_calls: u32,
    blocked: u32,
    edit_failures: u32,
    evidence: Option<TestEvidence>,
    edits_after_test: u32,
    execs_after_test: u32,
    files: BTreeSet<String>,
    bad_paths: BTreeSet<String>,
    writes: Writes,
    /// Each path's earliest content after the cut (none: absent).
    first: BTreeMap<String, Option<BlobRef>>,
    /// Each path's last content a tool left (outer none: not known).
    last: BTreeMap<String, Option<Option<BlobRef>>>,
    unknown: u32,
}

impl Tally {
    fn new(h: &Header) -> Tally {
        Tally {
            session_id: h.session_id.clone(),
            label: h.label.clone(),
            root: h.root.clone(),
            models: Vec::new(),
            user_turns: 0,
            steps: BTreeSet::new(),
            turn_start: None,
            wall_ms: 0,
            model: None,
            response: None,
            usage: Vec::new(),
            estimated: false,
            calls: HashMap::new(),
            tool_calls: 0,
            blocked: 0,
            edit_failures: 0,
            evidence: None,
            edits_after_test: 0,
            execs_after_test: 0,
            files: BTreeSet::new(),
            bad_paths: BTreeSet::new(),
            writes: Writes::default(),
            first: BTreeMap::new(),
            last: BTreeMap::new(),
            unknown: 0,
        }
    }

    fn model_used(&mut self, m: &ModelId) {
        if !self.models.contains(m) {
            self.models.push(m.clone());
        }
    }

    /// The open response is over: its last usage counts.
    fn close_response(&mut self) {
        if let Some(u) = self.response.take() {
            self.usage.push((self.model.clone(), u));
        }
    }

    /// `path` as shown, when it is valid under the session's root; else noted.
    fn path(&mut self, path: &tautly_session::RelPath) -> Option<String> {
        match path.resolve(&self.root) {
            Ok(_) => Some(path.as_str().to_string()),
            Err(_) => {
                self.bad_paths.insert(path.as_str().to_string());
                None
            }
        }
    }

    fn see(&mut self, t_ms: u64, rec: &SessionRecord, wrote: Vec<WriteValue>) {
        match rec {
            SessionRecord::TurnStart { .. } => {
                self.close_response();
                self.user_turns += 1;
                self.turn_start = Some(t_ms);
            }
            SessionRecord::TurnEnd { evidence, .. } => {
                self.close_response();
                if let Some(start) = self.turn_start.take() {
                    self.wall_ms = self.wall_ms.saturating_add(t_ms.saturating_sub(start));
                }
                if let Some(e) = evidence {
                    self.evidence = Some(e.clone());
                    self.edits_after_test = 0;
                    self.execs_after_test = 0;
                }
            }
            SessionRecord::Forked => self.close_response(),
            SessionRecord::Request { step, model, .. } => {
                self.close_response();
                self.steps.insert(*step);
                self.model = Some(model.clone());
                self.model_used(model);
            }
            SessionRecord::Provider(ProviderEvent::Usage(u)) => self.response = Some(*u),
            SessionRecord::Provider(ProviderEvent::DiscardedUsage(u)) => {
                self.usage.push((self.model.clone(), *u));
            }
            SessionRecord::Charged {
                model,
                usage,
                estimated,
                ..
            } => {
                self.model_used(model);
                self.usage.push((Some(model.clone()), *usage));
                self.estimated |= *estimated;
            }
            SessionRecord::ToolCall { id, name, .. } => {
                self.calls.insert(id.clone(), name.clone());
            }
            SessionRecord::ToolResult {
                id,
                is_error,
                effects,
                ran,
                blocked,
                ..
            } => {
                self.tool_calls += u32::from(*ran);
                self.blocked += u32::from(*blocked);
                if *is_error && self.calls.get(id).is_some_and(|n| n == EDIT_TOOL) {
                    self.edit_failures += 1;
                }
                for e in effects {
                    match e {
                        PersistedEffect::Edited { path, .. }
                        | PersistedEffect::Deleted { path, .. } => {
                            if let Some(p) = self.path(path) {
                                self.files.insert(p);
                            }
                        }
                        PersistedEffect::Exec { may_write: true } => self.execs_after_test += 1,
                        _ => {}
                    }
                }
            }
            SessionRecord::Edit { asserted: true, .. } => {}
            rec @ (SessionRecord::Edit { .. } | SessionRecord::Undone { .. }) => {
                if matches!(rec, SessionRecord::Edit { .. }) {
                    self.edits_after_test += 1;
                }
                for (path, before) in befores(rec) {
                    if let Some(p) = self.path(&path) {
                        self.first.entry(p.clone()).or_insert(before);
                        self.files.insert(p);
                    }
                }
                for (path, after) in wrote {
                    if path.resolve(&self.root).is_ok() {
                        self.last.insert(path.as_str().to_string(), after);
                    }
                }
            }
            SessionRecord::Unknown => self.unknown += 1,
            _ => {}
        }
    }

    fn finish(
        mut self,
        reader: &SessionReader<SessionRecord>,
        ledger: Option<&Ledger>,
        catalog: &Catalog,
        own_range: bool,
    ) -> BranchSummary {
        let mut notes = Vec::new();
        if !self.bad_paths.is_empty() {
            let n = self.bad_paths.len();
            notes.push(format!(
                "{n} path{} in the log {} not valid under the session's root (absolute, with \
                 `..` or a NUL byte) and {} left out",
                plural(n),
                if n == 1 { "is" } else { "are" },
                if n == 1 { "is" } else { "are" },
            ));
        }
        if self.unknown > 0 {
            let n = self.unknown as usize;
            notes.push(format!(
                "{n} record{} written by a newer version {} skipped",
                plural(n),
                if n == 1 { "was" } else { "were" }
            ));
        }
        let (added, removed) = self.diff_stat(reader, &mut notes);
        let mut evidence = self.evidence.take();
        if let Some(e) = &mut evidence
            && self.edits_after_test + self.execs_after_test > 0
        {
            e.stale = true;
            e.edits_since = e.edits_since.saturating_add(self.edits_after_test);
            e.execs_since = e.execs_since.saturating_add(self.execs_after_test);
        }
        let mut cost = self.logged_cost(catalog);
        if own_range && let Some(l) = ledger {
            match l.session_totals(&self.session_id) {
                Ok(t) if t.requests > 0 => {
                    cost = Cost {
                        tokens_in: t.input,
                        tokens_out: t.output,
                        cache_read: t.cache_read,
                        usd: (t.requests > t.unpriced).then_some(t.usd),
                        estimated: t.estimated > 0,
                        partial: t.unpriced > 0,
                    }
                }
                Ok(_) => {}
                Err(e) => notes.push(format!(
                    "the cost ledger could not be read ({e}); the cost is from the session log"
                )),
            }
        }
        BranchSummary {
            session_id: self.session_id,
            label: self.label,
            models: self.models,
            user_turns: self.user_turns,
            steps: u32::try_from(self.steps.len()).unwrap_or(u32::MAX),
            tokens_in: cost.tokens_in,
            tokens_out: cost.tokens_out,
            cache_read: cost.cache_read,
            cost_usd: cost.usd,
            cost_estimated: cost.estimated,
            cost_partial: cost.partial,
            wall_ms: self.wall_ms,
            tool_calls: self.tool_calls,
            blocked: self.blocked,
            edit_failures: self.edit_failures,
            evidence,
            files: self.files,
            added,
            removed,
            notes,
        }
    }

    /// The logged usage, each entry priced by its own model.
    fn logged_cost(&self, catalog: &Catalog) -> Cost {
        let mut c = Cost {
            estimated: self.estimated,
            ..Cost::default()
        };
        let mut usd = 0.0;
        let mut priced = 0;
        for (model, u) in &self.usage {
            c.tokens_in = c.tokens_in.saturating_add(u.input);
            c.tokens_out = c.tokens_out.saturating_add(u.output);
            c.cache_read = c.cache_read.saturating_add(u.cache_read);
            let price = model.as_ref().and_then(|m| catalog.get(m).price);
            match cost_usd(u, price.as_ref()) {
                Some(v) => {
                    usd += v;
                    priced += 1;
                }
                None => c.partial = true,
            }
        }
        c.usd = (priced > 0 || self.usage.is_empty()).then_some(usd);
        c
    }

    /// Lines added and removed: each path's earliest content after the cut against the
    /// last one a tool left there.
    fn diff_stat(
        &self,
        reader: &SessionReader<SessionRecord>,
        notes: &mut Vec<String>,
    ) -> (u32, u32) {
        let read = |b: &Option<BlobRef>| -> Result<Option<Vec<u8>>, ()> {
            b.as_ref()
                .map(|b| reader.read_blob(b))
                .transpose()
                .map_err(|_| ())
        };
        let mut changes = Vec::new();
        let (mut unknown, mut unreadable) = (0usize, 0usize);
        for (path, before) in &self.first {
            let Some(Some(after)) = self.last.get(path) else {
                unknown += 1;
                continue;
            };
            match (read(before), read(after)) {
                (Ok(b), Ok(a)) => changes.push((PathBuf::from(path), b, a)),
                _ => unreadable += 1,
            }
        }
        if unknown > 0 {
            notes.push(format!(
                "{unknown} file{} changed to content the log does not hold; {} lines are not \
                 counted",
                plural(unknown),
                if unknown == 1 { "its" } else { "their" }
            ));
        }
        if unreadable > 0 {
            notes.push(format!(
                "the content of {unreadable} file{} is missing from the session's blob store; \
                 {} lines are not counted",
                plural(unreadable),
                if unreadable == 1 { "its" } else { "their" }
            ));
        }
        let refs: Vec<FileChange<'_>> = changes
            .iter()
            .map(|(p, b, a)| (p.clone(), b.as_deref(), a.as_deref()))
            .collect();
        // Only the counts are wanted: no diff text is kept.
        let (_, counts) = diff_files(&refs, 0, 0);
        counts.iter().fold((0u32, 0u32), |(a, r), c| {
            (a.saturating_add(c.added), r.saturating_add(c.removed))
        })
    }
}

/// What `Writes::see` reports for a path.
type WriteValue = (tautly_session::RelPath, Option<Option<BlobRef>>);

#[derive(Default)]
struct Cost {
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    usd: Option<f64>,
    estimated: bool,
    partial: bool,
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Every file of `summaries`, sorted, with the indices of the summaries that touched it.
pub fn files_table(summaries: &[BranchSummary]) -> Vec<(String, Vec<usize>)> {
    let mut table: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in summaries.iter().enumerate() {
        for f in &s.files {
            table.entry(f).or_default().push(i);
        }
    }
    table
        .into_iter()
        .map(|(f, ix)| (f.to_string(), ix))
        .collect()
}
