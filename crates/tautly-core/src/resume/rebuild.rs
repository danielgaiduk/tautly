//! The rebuild of a session log into its context (see the module doc of [`super`]).

use std::collections::HashSet;

use tautly_context::TokenEstimator;
use tautly_context::pinned::{self, PinSource};
use tautly_context::{ContextStatus, GcElision, GcRecord, Turn};
use tautly_provider::{Content, Message, Role};
use tautly_session::{BlobRef, ConfineError};
use tautly_tools::{AppliedEffect, ContextOp, PersistedEffect, ToolResult};

use super::*;
use crate::dispatch::signature;
use crate::protocol::Command;
use crate::provider_setup::provider_label;
use crate::turn::Response;

/// [`rebuild`], calibrating the token estimator when `calibration` is given.
pub(crate) fn rebuild_with(
    reader: &SessionReader<SessionRecord>,
    until: Option<u32>,
    calibration: Option<&Calibration>,
) -> Result<Rebuilt, ResumeError> {
    let root = reader.header().root.clone();
    let model = reader.header().model.clone().map(ModelId::from);
    let mut r = Replay {
        reader,
        until,
        calibration,
        assemblies: HashMap::new(),
        out: Rebuilt {
            context: Context {
                root: Some(root.clone()),
                // As the session started: the history holds its first model's reasoning.
                reasoning_provider: model.as_ref().map(provider_label),
                ..Context::default()
            },
            incomplete_turn: false,
            turn_count: 0,
            warnings: Vec::new(),
            attempts: AttemptLedger::default(),
            latest_compaction: None,
            prefix: None,
            budgets: Vec::new(),
            usage: Vec::new(),
            end_offset: None,
            model,
            reasoning_retry: false,
            checkpoints: LoggedCheckpoints::default(),
            seen: HashMap::new(),
            start_pins: false,
            memory_proposals: Vec::new(),
            memory_seq: 0,
            gc_elided: Vec::new(),
        },
        root,
        step: None,
        running: false,
        started: false,
        pending: Vec::new(),
        results: HashMap::new(),
        logged_tools: None,
        unknown: 0,
        restoring: None,
    };
    let mut stopped = false;
    for rec in reader.records() {
        let (offset, rec) = rec?;
        match r.apply(rec) {
            Flow::Go => r.out.end_offset = Some(offset),
            Flow::Skip => {}
            Flow::Stop => {
                stopped = true;
                break;
            }
        }
    }
    r.close_step();
    // Pins of a turn the log ends in (or the cut closes) reach the context as its
    // leftovers would have.
    r.apply_pending();
    if let Some((backup, paths, kept, own)) = r.restoring.take() {
        let id = &reader.header().session_id;
        let names: Vec<&str> = paths.iter().map(tautly_session::RelPath::as_str).collect();
        let list = names.join(" ");
        let restore_cmd = format!("`tautly sessions restore-backup {id}`");
        r.warn(match backup {
            _ if own => format!(
                "{restore_cmd} was cut off while it wrote {list}: running it again finishes \
                 putting back the backup of the rewind or switch before it; with \
                 --include-own it instead puts back what it replaced, which it kept in the \
                 session log"
            ),
            Some(b) => {
                let mut text = format!(
                    "a rewind or switch was cut off while it wrote {list}; their content from \
                     before it is in the snapshot {b}: compare with `git diff {b} -- {list}` \
                     and put back only those paths with `git restore --source {b} -- {list}` \
                     (later edits of other files are kept)"
                );
                if kept {
                    text.push_str(&format!(
                        "; files the snapshot cannot hold are put back by {restore_cmd}"
                    ));
                }
                text
            }
            None => format!(
                "a rewind or switch was cut off while it wrote {list}; their content from \
                 before it is kept in the session log: put it back with {restore_cmd}"
            ),
        });
    }
    if r.unknown > 0 {
        let n = r.unknown;
        r.warn(format!(
            "{n} record{} written by a newer version {} skipped; the rebuilt context may \
             differ from the one that version had",
            if n == 1 { "" } else { "s" },
            if n == 1 { "was" } else { "were" }
        ));
    }
    if !stopped {
        let last = r.out.context.next_turn() - 1;
        if let Some(at) = until.filter(|&at| at > last) {
            return Err(ResumeError::NoSuchStep { at, last });
        }
    }
    r.out.incomplete_turn = r.running;
    Ok(r.out)
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// Replayed: part of the state.
    Go,
    /// Replayed, but changes nothing (a command such as `Fork` or `SendMessage`).
    Skip,
    /// Past the cut: the replay ends before this record.
    Stop,
}

/// A step being rebuilt: its response so far and the results of its calls.
struct OpenStep {
    number: u32,
    /// The model of its `Request`.
    model: ModelId,
    resp: Response,
    responded: bool,
    results: HashMap<String, Content>,
    /// The request as assembled from the rebuilt context (with a calibration).
    status: Option<ContextStatus>,
}

struct Replay<'a> {
    reader: &'a SessionReader<SessionRecord>,
    until: Option<u32>,
    calibration: Option<&'a Calibration>,
    /// What requests were assembled with, per model (with a calibration).
    assemblies: HashMap<ModelId, (Capabilities, Vec<ToolSpec>)>,
    out: Rebuilt,
    root: PathBuf,
    step: Option<OpenStep>,
    /// Between a `TurnStart` and its `TurnEnd`.
    running: bool,
    /// A `TurnStart` was replayed: records before the first one are session set-up, which
    /// no cut leaves out.
    started: bool,
    /// `Pin`/`Unpin` sent while the turn ran, for its next step start.
    pending: Vec<Command>,
    /// The result blob of every call, for an `Uncompacted` restore.
    results: HashMap<String, BlobRef>,
    /// The tools the last `PrefixApplied` logged (with a calibration): the requests after
    /// it were assembled with them. None: assembled from the registry (an older log).
    logged_tools: Option<Vec<ToolSpec>>,
    /// Records of a type this version does not know, skipped.
    unknown: usize,
    /// A rewind or switch (or `restore-backup`) began writing files and its end is not
    /// logged: the backup ref it named, if any, the paths, whether the log kept files, and
    /// whether `restore-backup` wrote it.
    restoring: Option<(Option<String>, Vec<tautly_session::RelPath>, bool, bool)>,
}

impl Replay<'_> {
    fn ctx(&mut self) -> &mut Context {
        &mut self.out.context
    }

    fn warn(&mut self, message: String) {
        self.out.warnings.push(message);
    }

    /// The boundary of `until` lies behind: the next step would be past it.
    fn beyond(&self) -> bool {
        self.until.is_some_and(|n| self.out.context.next_turn() > n)
    }

    fn apply(&mut self, rec: SessionRecord) -> Flow {
        match rec {
            SessionRecord::Provider(ev) => {
                // A tool call's input kept in a blob is read back.
                let (ev, unread) = crate::record::resolved_event(self.reader, ev);
                if let Some(e) = unread {
                    self.warn(format!(
                        "a tool call's input could not be read back ({e}); it is rebuilt as \
                         an empty object"
                    ));
                }
                if let Some(step) = &mut self.step {
                    step.resp.push(ev);
                    step.responded = true;
                }
            }
            SessionRecord::ToolResult {
                id,
                result_blob,
                is_error,
                effects,
                meta,
                ran,
                blocked,
            } => self.tool_result(id, result_blob, is_error, &effects, meta, ran, blocked),
            SessionRecord::Command(cmd) => return self.command(cmd),
            SessionRecord::TurnStart { turn, text } => {
                self.close_step();
                if self.beyond() {
                    return Flow::Stop;
                }
                self.ctx().history.push(Turn {
                    messages: vec![Message {
                        role: Role::User,
                        content: vec![Content::Text(text)],
                    }],
                });
                self.out.turn_count = self.out.turn_count.max(turn);
                self.running = true;
                self.started = true;
            }
            SessionRecord::TurnEnd { .. } => {
                self.close_step();
                self.apply_pending();
                self.running = false;
            }
            SessionRecord::MemoryProposal { id, change } => {
                self.out.memory_seq += 1;
                self.out.memory_proposals.push((id, change));
            }
            SessionRecord::MemoryDecision { id, .. } => {
                self.out.memory_proposals.retain(|(p, _)| *p != id);
            }
            SessionRecord::Forked => {
                self.close_step();
                self.out.usage.clear();
            }
            SessionRecord::ModelSet { model } => {
                self.close_step();
                if self.running && self.beyond() {
                    return Flow::Stop;
                }
                self.apply_pending();
                self.switch(model);
            }
            // The refused request's retry is sent without any reasoning.
            SessionRecord::ReasoningStripped => self.ctx().strip_reasoning(None),
            SessionRecord::Request { step, model, .. } => {
                self.close_step();
                if self.beyond() {
                    return Flow::Stop;
                }
                self.apply_pending();
                let next = self.out.context.next_turn();
                if step != next {
                    self.warn(format!(
                        "the log's step {step} was rebuilt as step {next}; later step numbers \
                         may not match the log"
                    ));
                }
                // Every step start strips another provider's reasoning, and takes the
                // switch's retry and forced cold epoch.
                let ctx = &mut self.out.context;
                ctx.strip_reasoning(Some(&provider_label(&model)));
                ctx.cold_epoch_forced = false;
                self.out.reasoning_retry = false;
                let status = self.calibration.map(|c| {
                    let (caps, specs) = self
                        .assemblies
                        .entry(model.clone())
                        .or_insert_with(|| c.assembly(&model));
                    let specs = self.logged_tools.as_ref().unwrap_or(specs);
                    self.out.context.assemble(caps, specs, &model).1
                });
                self.step = Some(OpenStep {
                    number: next,
                    model: model.clone(),
                    resp: Response::default(),
                    responded: false,
                    results: HashMap::new(),
                    status,
                });
            }
            rec @ (SessionRecord::GcApplied { .. }
            | SessionRecord::Compaction { .. }
            | SessionRecord::PinsFolded
            | SessionRecord::MemoryIndexApplied { .. }
            | SessionRecord::PrefixApplied { .. }
            | SessionRecord::StartPins { .. }
            | SessionRecord::Uncompacted { .. }
            | SessionRecord::Undone { .. }) => {
                self.close_step();
                // An epoch at the start of a step past the cut; one between turns (or
                // before the first) is part of the cut. An undo past the cut is not: the
                // files at the cut (`state_at`) are the ones before it.
                let undo = matches!(rec, SessionRecord::Undone { .. });
                if (self.running || undo) && self.beyond() {
                    return Flow::Stop;
                }
                self.apply_pending();
                self.mutation(rec);
            }
            SessionRecord::Charged { model, usage, .. } => self.out.usage.push((model, usage)),
            // The files changed unseen: the write generation moves. One noticed at a
            // step start (`late`) belongs to that step, like its epoch.
            SessionRecord::WritesLost { paths, late } => {
                if late {
                    self.close_step();
                    if self.running && self.beyond() {
                        return Flow::Stop;
                    }
                }
                for p in &paths {
                    if let Ok(path) = p.resolve(&self.root) {
                        self.out.seen.remove(&path);
                    }
                }
                self.out
                    .attempts
                    .note_effects(&[AppliedEffect::Exec { may_write: true }]);
            }
            SessionRecord::Checkpoint {
                n, commit, tree, ..
            } => {
                let logged = &mut self.out.checkpoints;
                logged.max_n = logged.max_n.max(n);
                logged.all.push((n, commit.clone()));
                logged.last = Some((n, commit, tree));
            }
            SessionRecord::ToolCall { .. }
            | SessionRecord::Edit { .. }
            | SessionRecord::ApprovalRequested { .. }
            | SessionRecord::CheckpointSkipped { .. }
            | SessionRecord::Labeled { .. }
            | SessionRecord::EngineError { .. } => {}
            SessionRecord::RestoreBegun {
                backup_ref,
                files,
                paths,
                by,
            } => {
                let own = by == crate::record::RestoreBy::RestoreBackup;
                self.restoring = Some((backup_ref, paths, !files.is_empty(), own))
            }
            SessionRecord::RestoreEnded { .. } => self.restoring = None,
            SessionRecord::Unknown | SessionRecord::Unreadable => self.unknown += 1,
        }
        Flow::Go
    }

    fn command(&mut self, cmd: Command) -> Flow {
        match cmd {
            Command::SetBudget {
                scope,
                max_usd,
                max_tokens,
            } if scope != BudgetScope::Unknown
                && max_usd.is_none_or(|usd| usd.is_finite() && usd >= 0.0) =>
            {
                self.out.budgets.push((
                    scope,
                    Budget {
                        max_usd,
                        max_tokens,
                    },
                ));
            }
            cmd @ (Command::Pin { .. } | Command::Unpin { .. }) if self.running => {
                self.pending.push(cmd);
            }
            // Sent between turns: part of what the next step is built from.
            cmd @ (Command::Pin { .. } | Command::Unpin { .. }) => {
                self.close_step();
                context_command(self.ctx(), cmd);
            }
            _ => return Flow::Skip,
        }
        Flow::Go
    }

    /// A `ModelSet`, as `Session::set_model` changed the context.
    fn switch(&mut self, model: ModelId) {
        let label = provider_label(&model);
        let same_provider = self.out.model.as_ref().map(provider_label) == Some(label.clone());
        let ctx = &mut self.out.context;
        ctx.strip_reasoning(Some(&label));
        if !same_provider {
            ctx.tokens = TokenEstimator::default();
        }
        ctx.force_cold_epoch();
        self.out.reasoning_retry = same_provider;
        self.out.model = Some(model);
    }

    fn apply_pending(&mut self) {
        for cmd in std::mem::take(&mut self.pending) {
            context_command(&mut self.out.context, cmd);
        }
    }

    /// An epoch, set-up or user record that changes the context.
    fn mutation(&mut self, rec: SessionRecord) {
        match rec {
            SessionRecord::GcApplied {
                elided,
                tokens_freed,
                ..
            } => {
                self.out.gc_elided.push((
                    elided.iter().map(|(id, _)| id.clone()).collect(),
                    tokens_freed,
                ));
                let unknown = self.ctx().reapply_gc(&elided);
                if unknown > 0 {
                    self.warn(format!(
                        "{unknown} collected results were not found in the rebuilt history"
                    ));
                }
            }
            SessionRecord::Compaction { removed, summary } => {
                match self.ctx().reapply_compaction(removed.len(), summary) {
                    Ok(rec) => self.out.latest_compaction = Some(rec),
                    Err(e) => self.warn(format!("a logged compaction was not redone: {e}")),
                }
            }
            SessionRecord::PinsFolded => {
                self.ctx().fold_session_pins();
            }
            SessionRecord::MemoryIndexApplied { text } => {
                self.ctx().memory_index = Some(text).filter(|t| !t.trim().is_empty());
            }
            SessionRecord::PrefixApplied {
                agents_md,
                repo_map,
                system_prompt_hash,
                tools_hash,
                tools,
            } => {
                let agents_text = self.text(agents_md.as_ref(), "AGENTS.md");
                let map_text = self.text(repo_map.as_ref(), "repo map");
                self.ctx().agents_md = agents_text;
                self.ctx().repo_map = map_text;
                if self.calibration.is_some() {
                    self.logged_tools = tools.as_ref().and_then(|b| self.tool_specs(b));
                }
                self.out.prefix = Some(SessionRecord::PrefixApplied {
                    agents_md,
                    repo_map,
                    system_prompt_hash,
                    tools_hash,
                    tools,
                });
            }
            SessionRecord::StartPins { pins } => {
                for p in &pins {
                    pinned::add(&mut self.out.context.pinned, p.into());
                }
                self.out.start_pins = true;
            }
            SessionRecord::Uncompacted {
                compaction,
                restored,
            } => {
                if compaction {
                    match self.out.latest_compaction.take() {
                        Some(rec) => {
                            if let Err(e) = self.ctx().uncompact(&rec, u32::MAX) {
                                self.warn(format!("a logged uncompact was not redone: {e}"));
                            }
                        }
                        None => self.warn("a logged uncompact has no compaction to undo".into()),
                    }
                }
                let mut elided = Vec::with_capacity(restored.len());
                for id in restored {
                    let original = match self.results.get(&id).map(|b| self.reader.read_blob(b)) {
                        Some(Ok(bytes)) => bytes,
                        Some(Err(e)) => {
                            self.warn(format!(
                                "the uncompacted result of call {id} could not be read ({e}); \
                                 it stays collected"
                            ));
                            continue;
                        }
                        None => {
                            self.warn(format!(
                                "the uncompacted result of call {id} is not in the log; it \
                                 stays collected"
                            ));
                            continue;
                        }
                    };
                    elided.push(GcElision {
                        call_id: id,
                        original: String::from_utf8_lossy(&original).into_owned(),
                        by_turn: 0,
                    });
                }
                restore_and_keep(self.ctx(), elided);
            }
            SessionRecord::Undone {
                files, backup_ref, ..
            } => {
                if let Some(n) = backup_ref
                    .as_deref()
                    .and_then(|r| r.rsplit_once("/undo-"))
                    .and_then(|(_, n)| n.parse::<u32>().ok())
                {
                    let logged = &mut self.out.checkpoints;
                    logged.max_n = logged.max_n.max(n);
                }
                for f in &files {
                    let Ok(path) = f.path.resolve(&self.root) else {
                        continue;
                    };
                    if let Some(note) = self.out.context.state.files.get_mut(&path) {
                        note.maybe_stale = true;
                    }
                    self.out.seen.remove(&path);
                }
                self.out
                    .attempts
                    .note_effects(&[AppliedEffect::Exec { may_write: true }]);
            }
            _ => {}
        }
    }

    /// The text of a prefix blob (none: absent or unreadable, which is said).
    fn text(&mut self, blob: Option<&BlobRef>, what: &str) -> Option<String> {
        match self.reader.read_blob(blob?) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                self.warn(format!(
                    "the logged {what} could not be read ({e}); left out"
                ));
                None
            }
        }
    }

    /// The tool specs of a `PrefixApplied` blob (none: unreadable, which is said; the
    /// requests are then assembled from the registry).
    fn tool_specs(&mut self, blob: &BlobRef) -> Option<Vec<ToolSpec>> {
        let parsed = self
            .reader
            .read_blob(blob)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Vec<ToolSpec>>(&b).map_err(|e| e.to_string()));
        match parsed {
            Ok(specs) => Some(specs),
            Err(e) => {
                self.warn(format!(
                    "the logged tools could not be read ({e}); the estimate is rebuilt with \
                     this version's"
                ));
                None
            }
        }
    }

    /// A result of the open step, replayed as the live loop settled it.
    #[allow(clippy::too_many_arguments)]
    fn tool_result(
        &mut self,
        id: String,
        blob: BlobRef,
        is_error: bool,
        effects: &[PersistedEffect],
        meta: tautly_tools::ToolMeta,
        ran: bool,
        blocked: bool,
    ) {
        let Some(call) = self.step.as_ref().and_then(|s| {
            s.resp
                .calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| (c.name.clone(), c.input()))
        }) else {
            self.warn(format!(
                "the result of call {id} has no call before it; left out"
            ));
            return;
        };
        let (name, args) = call;
        let output = match self.reader.read_blob(&blob) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                self.warn(format!("the result of call {id} could not be read ({e})"));
                format!("[this result could not be read back from the session log: {e}]")
            }
        };
        self.results.insert(id.clone(), blob);
        // Only paths and hashes are replayed; the file contents stay in their blobs.
        let mut applied = Vec::with_capacity(effects.len());
        for e in effects {
            match e.to_applied(&self.root, &mut |_| Ok::<_, ConfineError>(Vec::new())) {
                Ok(a) => applied.push(a),
                Err(err) => self.warn(format!("an effect of call {id} was left out: {err}")),
            }
        }
        let number = self.step.as_ref().map_or(0, |s| s.number);
        let r = &mut self.out;
        // Live, every call that ran or was blocked went through `check` first, which also
        // spends the escape hatch. (A call the permission policy denied did too; the log
        // does not say which those were.)
        if ran || blocked {
            r.attempts.check(&name, &args);
        }
        if ran {
            if !is_error
                && let Some(op) = tautly_tools::parse_context_op(&name, &args)
                && !matches!(op, ContextOp::Remember(_))
            {
                let _ = r.context.apply_op(op);
            }
            r.attempts.note_effects(&applied);
            let result = ToolResult {
                output: output.clone(),
                is_error,
                effects: Vec::new(),
                meta,
                applied_diff: None,
            };
            r.attempts.record(
                &name,
                &args,
                number,
                !is_error,
                &signature(&name, &result),
                &output,
            );
        }
        for e in &applied {
            match e {
                AppliedEffect::Read {
                    path, file_hash, ..
                } => {
                    r.seen.insert(path.clone(), *file_hash);
                }
                AppliedEffect::Edited {
                    path, post_hash, ..
                } => {
                    r.seen.insert(path.clone(), *post_hash);
                }
                AppliedEffect::Deleted { path, .. } => {
                    r.seen.remove(path);
                }
                AppliedEffect::Exec { .. } => {}
            }
        }
        r.context.apply_effects(&applied, number);
        r.context
            .push_tool_result(&id, &name, &args, &applied, is_error, number);
        if let Some(step) = &mut self.step {
            step.results.insert(
                id.clone(),
                Content::ToolResult {
                    id,
                    output,
                    is_error,
                },
            );
        }
    }

    /// The open step is over: push its turn, as the live loop did once its calls ran.
    fn close_step(&mut self) {
        let Some(mut step) = self.step.take() else {
            return;
        };
        if !step.responded {
            // The request got no complete response: the step left nothing behind.
            return;
        }
        let resp = step.resp.finish();
        let r = &mut self.out;
        if let Some(u) = resp.usage {
            if let Some(status) = &step.status {
                r.context.calibrate(status, &u);
            }
            r.usage.push((step.model.clone(), u));
        }
        r.usage
            .extend(resp.discarded.iter().map(|u| (step.model.clone(), *u)));
        let assistant = Message {
            role: Role::Assistant,
            content: resp.content,
        };
        if resp.calls.is_empty() {
            if !assistant.content.is_empty() {
                r.context.history.push(Turn {
                    messages: vec![assistant],
                });
            }
            return;
        }
        let mut results = Vec::with_capacity(resp.calls.len());
        for c in &resp.calls {
            match step.results.remove(&c.id) {
                Some(content) => results.push(content),
                None => {
                    r.context
                        .push_tool_result(&c.id, &c.name, &c.input(), &[], true, step.number);
                    results.push(Content::ToolResult {
                        id: c.id.clone(),
                        output: OPEN_CALL_RESULT.to_string(),
                        is_error: true,
                    });
                }
            }
        }
        r.context.history.push(Turn {
            messages: vec![
                assistant,
                Message {
                    role: Role::User,
                    content: results,
                },
            ],
        });
        r.context.state.failed_attempts = r.attempts.listed();
    }
}

/// `Pin`/`Unpin` as the session applies them (refusals change nothing).
fn context_command(ctx: &mut Context, cmd: Command) {
    match cmd {
        Command::Pin { text } => {
            let _ = ctx.pin(text, PinSource::User);
        }
        Command::Unpin { text } => {
            ctx.pinned.retain(|p| p.text != text);
            ctx.session_pins.retain(|p| p.text != text);
        }
        _ => {}
    }
}

/// Put collected outputs back and keep them from being collected again for what
/// superseded them so far (a later turn's result can); the call ids restored.
pub(super) fn restore_and_keep(ctx: &mut Context, elided: Vec<GcElision>) -> Vec<String> {
    let now = ctx.next_turn().saturating_sub(1);
    let ids: HashSet<String> = elided.iter().map(|e| e.call_id.clone()).collect();
    ctx.restore_gc(&GcRecord {
        elided,
        tokens_freed: 0,
        reason: MutationReason::Manual,
        forgotten_paths: Vec::new(),
    });
    let mut restored = Vec::new();
    for meta in ctx.results.iter_mut() {
        if ids.contains(&meta.call_id) && !meta.elided {
            meta.kept_until = Some(now);
            restored.push(meta.call_id.clone());
        }
    }
    restored
}

/// The original outputs of `ids`, from their `ToolResult` blobs in the log at `path`.
pub(super) fn originals(
    path: &std::path::Path,
    ids: &[String],
) -> Result<Vec<GcElision>, ResumeError> {
    let reader = SessionReader::<SessionRecord>::open(path)?;
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut blobs: HashMap<String, BlobRef> = HashMap::new();
    for rec in reader.records() {
        if let (
            _,
            SessionRecord::ToolResult {
                id, result_blob, ..
            },
        ) = rec?
            && wanted.contains(id.as_str())
        {
            blobs.insert(id, result_blob);
        }
    }
    let mut out = Vec::with_capacity(blobs.len());
    for id in ids {
        if let Some(blob) = blobs.get(id) {
            out.push(GcElision {
                call_id: id.clone(),
                original: String::from_utf8_lossy(&reader.read_blob(blob)?).into_owned(),
                by_turn: 0,
            });
        }
    }
    Ok(out)
}
