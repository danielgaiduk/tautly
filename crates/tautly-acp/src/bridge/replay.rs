//! Diffs of finished edits, and a loaded session's history replayed from its log.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};
use tautly_core::{DiffPreview, EffectView, Event, SessionReader, SessionRecord};

use super::events::{Session, chunk};
use crate::map::{self};
use crate::schema::{ContentBlock, ToolCall, ToolCallContent, ToolCallStatus};

/// The `diff` contents of a finished edit: from its applied diff and the files on disk
/// when they are still as the call wrote them (the common case, no log read); else from
/// the texts the session log holds for the call (a truncated diff, a file changed since);
/// else the applied unified diff as text (a file too large or not text).
pub(super) async fn finished_diffs(
    s: &Session,
    call_id: &str,
    effects: &[EffectView],
    diff: Option<DiffPreview>,
) -> Vec<ToolCallContent> {
    let log = s.engine.current_session().1;
    let (root, id) = (s.root.clone(), call_id.to_string());
    let written: Vec<map::Written> = effects
        .iter()
        .filter_map(|e| match e {
            EffectView::Edited {
                path, post_hash, ..
            } => Some((path.clone(), Some(post_hash.clone()))),
            EffectView::Deleted { path } => Some((path.clone(), None)),
            _ => None,
        })
        .collect();
    let applied = diff
        .as_ref()
        .filter(|d| !d.truncated)
        .map(|d| d.unified.clone());
    let texts = tokio::task::spawn_blocking(move || {
        if let Some(found) = applied.and_then(|u| map::applied_diffs(&root, &u, &written)) {
            return Some(found);
        }
        let files = call_edits(&log, &id)?;
        files
            .into_iter()
            .map(|(rel, pre, post)| {
                let old = pre.map(String::from_utf8).transpose().ok()?;
                let new = post
                    .map(String::from_utf8)
                    .transpose()
                    .ok()?
                    .unwrap_or_default();
                map::diff_content(&map::absolute(&root, &rel), old.as_deref(), &new)
            })
            .collect::<Option<Vec<_>>>()
    })
    .await
    .ok()
    .flatten()
    .filter(|v| !v.is_empty());
    match (texts, diff) {
        (Some(t), _) => t,
        (None, Some(d)) => vec![ToolCallContent::text(map::fenced(&d.unified))],
        (None, None) => Vec::new(),
    }
}

/// A file one call changed: root-relative path, content before (None: created) and after
/// (None: deleted).
type FileChange = (String, Option<Vec<u8>>, Option<Vec<u8>>);

/// The files call `id` changed, from the `Edit` records that follow its `ToolResult` in
/// the log at `path`; None when the log does not have them (or a blob is missing).
fn call_edits(path: &Path, id: &str) -> Option<Vec<FileChange>> {
    let reader = SessionReader::<SessionRecord>::open(path).ok()?;
    let mut found: Option<Vec<FileChange>> = None;
    let mut collecting = false;
    for rec in reader.records() {
        let (_, rec) = rec.ok()?;
        match rec {
            SessionRecord::ToolResult { id: rid, .. } => {
                collecting = rid == id;
                if collecting {
                    found = Some(Vec::new());
                }
            }
            SessionRecord::Edit {
                path,
                pre_image,
                post_image_blob,
                asserted: false,
                ..
            } if collecting => {
                let pre = match &pre_image {
                    Some(b) => Some(reader.read_blob(b).ok()?),
                    None => None,
                };
                let post = match &post_image_blob {
                    Some(b) => Some(reader.read_blob(b).ok()?),
                    None => None,
                };
                found.as_mut()?.push((path.as_str().to_string(), pre, post));
            }
            _ => collecting = false,
        }
    }
    found
}

/// The updates that replay a resumed session (spec 7.11.9): its user messages, the
/// agent's messages and its tool calls in their final state, from core's playback mapping
/// ([`tautly_core::replay::events`]) with each finished edit's diff from the `Edit` records
/// behind its result (a rewind's asserted contents left out, as in turn diffs). `out`
/// returning false (nobody reads any more) stops it. Blocking file I/O.
pub(super) fn replay(log: &Path, root: &Path, out: &mut dyn FnMut(Value) -> bool) {
    let Ok(reader) = SessionReader::<SessionRecord>::open(log) else {
        return;
    };
    // Each call's `Edit` records (blob references only), read ahead of the events.
    let mut edits: HashMap<String, Vec<SessionRecord>> = HashMap::new();
    let mut after: Option<String> = None;
    for rec in reader.records() {
        let Ok((_, rec)) = rec else {
            break;
        };
        match rec {
            SessionRecord::ToolResult { id, .. } => after = Some(id),
            rec @ SessionRecord::Edit {
                asserted: false, ..
            } => {
                if let Some(id) = &after {
                    edits.entry(id.clone()).or_default().push(rec);
                }
            }
            _ => after = None,
        }
    }
    let text = |b: &Option<_>| match b {
        Some(b) => reader
            .read_blob(b)
            .ok()
            .and_then(|v| String::from_utf8(v).ok())
            .map(Some),
        None => Some(None),
    };
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    for (_, _, ev) in tautly_core::replay::events(&reader, None) {
        let update = match ev {
            Event::UserMessage { text } => json!({
                "sessionUpdate": "user_message_chunk",
                "content": ContentBlock::text(text),
            }),
            Event::TextDelta { text } => chunk(&text),
            Event::ToolCallProposed {
                call_id,
                tool,
                args,
            } => {
                calls.insert(call_id, (tool, args));
                continue;
            }
            Event::ToolFinished {
                call_id,
                output,
                is_error,
                blocked,
                ..
            } => {
                let (tool, args) = calls.remove(&call_id).unwrap_or_default();
                let mut content = vec![map::output_content(&output)];
                for rec in edits.remove(&call_id).unwrap_or_default() {
                    if let SessionRecord::Edit {
                        path,
                        pre_image,
                        post_image_blob,
                        ..
                    } = &rec
                        && let (Some(old), Some(new)) = (text(pre_image), text(post_image_blob))
                        && let Some(d) = map::diff_content(
                            &map::absolute(root, path.as_str()),
                            old.as_deref(),
                            new.as_deref().unwrap_or(""),
                        )
                    {
                        content.push(d);
                    }
                }
                let call = ToolCall {
                    tool_call_id: call_id,
                    title: map::tool_title(&tool, &args),
                    kind: map::tool_kind(&tool),
                    status: if is_error || blocked {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    },
                    content,
                    locations: map::locations(root, &args),
                    raw_input: Some(args),
                };
                let mut v = serde_json::to_value(call).unwrap_or_default();
                v["sessionUpdate"] = json!("tool_call");
                v
            }
            _ => continue,
        };
        if !out(update) {
            return;
        }
    }
}

/// What session `id` has cost so far, from the cost ledger under `data_home` (None: no
/// ledger, or nothing priced). Blocking.
pub(super) fn ledger_spend(data_home: &Path, id: &str) -> Option<f64> {
    let file = data_home.join(tautly_core::ledger::LEDGER_FILE);
    if !file.exists() {
        return None;
    }
    let totals = tautly_core::Ledger::open(&file)
        .ok()?
        .session_totals(id)
        .ok()?;
    (totals.requests > totals.unpriced).then_some(totals.usd)
}
