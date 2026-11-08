//! The session tree (spec 7.12.3): a project's sessions as branches of each other, for
//! `session/tree` (RPC), `tautly sessions tree` and `/tree`.
//!
//! Each node is read from its own log: the header gives its parent (session, record
//! offset), label and origin; the offset becomes the step it branched at by
//! [`step_at_offset`](crate::timetravel::step_at_offset) on the branch's own log (its
//! records up to that offset are a verbatim copy of the parent's), so a branch whose
//! parent was deleted still shows where it came from. A session is live when an engine
//! holds its log's writer lock.

use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tautly_session::Origin;

use crate::ledger::{LEDGER_FILE, Ledger};
use crate::record::{RecordLog, SessionRecord};
use crate::timetravel::TtError;

/// Longest [`TreeNode::last_prompt`] kept, in characters.
const PROMPT_KEPT: usize = 200;
/// Longest prompt a tree line shows, in characters.
const PROMPT_SHOWN: usize = 40;

/// One session of the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    pub session_id: String,
    /// The session it branched from and the step it branched at.
    pub parent: Option<(String, u32)>,
    /// Its label: the last `Labeled` record's, else the header's.
    pub label: Option<String>,
    pub origin: Origin,
    /// When it started, in Unix seconds.
    pub created: u64,
    /// Its last step's number (spec 7.12.1; not a count of provider steps).
    pub last_step: u32,
    /// The first line of its last user message (at most 200 characters; empty: none).
    pub last_prompt: String,
    /// What it spent, from the cost ledger (none: no priced request).
    pub cost_usd: Option<f64>,
    /// An engine has it open.
    pub live: bool,
    /// It is the session `build` was told is current.
    pub current: bool,
}

/// The sessions of project `project` (its [`project_id`](tautly_session::paths::project_id))
/// under `data_home`, as a tree in depth-first order: each root (no parent, or a parent no
/// longer listed) by start time, each followed by its branches by start time. Logs that
/// cannot be read are left out. Blocking I/O (every log, the cost ledger).
pub fn build(
    data_home: &Path,
    project: &str,
    current: Option<&str>,
) -> Result<Vec<TreeNode>, TtError> {
    let dir = tautly_session::paths::sessions_dir(data_home, project);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(TtError::Session(e.into())),
    };
    let ledger_file = data_home.join(LEDGER_FILE);
    let ledger = ledger_file
        .exists()
        .then(|| Ledger::open(&ledger_file).ok())
        .flatten();
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(mut node) = cached_node(&path) {
            node.live = held(&path);
            node.current = current == Some(node.session_id.as_str());
            node.cost_usd = ledger
                .as_ref()
                .and_then(|l| l.session_totals(&node.session_id).ok())
                .filter(|t| t.requests > t.unpriced)
                .map(|t| t.usd);
            nodes.push(node);
        }
    }
    Ok(order(nodes))
}

/// [`build`] for the project of the work tree a session started in `start` would run on
/// ([`session_root`](crate::session_root)).
pub fn for_root(
    data_home: &Path,
    start: &Path,
    current: Option<&str>,
) -> Result<Vec<TreeNode>, TtError> {
    let root = crate::session_root(start).map_err(|e| TtError::Other(e.to_string()))?;
    build(
        data_home,
        &tautly_session::paths::project_id(&root),
        current,
    )
}

/// Nodes already read, by log, with the length and modification time they were read at:
/// a log that has not changed is not read again (cp5 review C M7).
/// shortcut: in-process only (the REPL and RPC server repeat `/tree`; a one-shot
/// `tautly sessions tree` reads every log), upgrade to a file under the data home if that
/// becomes slow.
static NODES: std::sync::Mutex<Option<NodeCache>> = std::sync::Mutex::new(None);

/// Each log's node, with the log's length and modification time when it was read.
type NodeCache = HashMap<std::path::PathBuf, (u64, std::time::SystemTime, TreeNode)>;

/// [`node`], from [`NODES`] when the log is unchanged.
fn cached_node(path: &Path) -> Option<TreeNode> {
    let meta = std::fs::metadata(path).ok()?;
    let key = (meta.len(), meta.modified().ok()?);
    let mut cache = crate::lock(&NODES);
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((len, mtime, node)) = cache.get(path)
        && (*len, *mtime) == key
    {
        return Some(node.clone());
    }
    let node = node(path)?;
    cache.insert(path.to_path_buf(), (key.0, key.1, node.clone()));
    Some(node)
}

/// One log's node (cost, `live` and `current` filled in by the caller); none when it
/// cannot be read.
fn node(path: &Path) -> Option<TreeNode> {
    let reader = RecordLog::open(path).ok()?;
    let header = reader.header().clone();
    let mut label = header.label.clone();
    let mut last_prompt = String::new();
    // One pass: the steps, the step at the parent offset (when the header does not name
    // it), the label and the last prompt.
    let mut steps = crate::timetravel::StepCounter::default();
    let fork_at = header
        .parent
        .as_ref()
        .filter(|_| header.parent_step.is_none())
        .map(|(_, offset)| *offset);
    let mut parent_step = 0;
    for rec in reader.records() {
        let Ok((at, rec)) = rec else {
            break;
        };
        steps.see(&rec);
        if fork_at.is_some_and(|o| at <= o) {
            parent_step = steps.last();
        }
        match rec {
            SessionRecord::Labeled { label: l } => label = Some(l),
            SessionRecord::TurnStart { text, .. } => {
                let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                last_prompt = line.trim().chars().take(PROMPT_KEPT).collect();
            }
            _ => {}
        }
    }
    let last_step = steps.last();
    let parent = header
        .parent
        .map(|(id, _)| (id, header.parent_step.unwrap_or(parent_step)));
    Some(TreeNode {
        session_id: header.session_id,
        parent,
        label,
        origin: header.origin,
        created: header
            .created
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        last_step,
        last_prompt,
        cost_usd: None,
        live: false,
        current: false,
    })
}

/// Whether a writer (an engine) holds the log's lock.
fn held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // The lock goes with the file.
    matches!(
        crate::lease::flock(&file, libc::LOCK_SH | libc::LOCK_NB),
        Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK)
    )
}

/// Depth-first: roots by start time, each followed by its branches.
fn order(mut nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    nodes.sort_by(|a, b| {
        a.created
            .cmp(&b.created)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let ids: std::collections::HashSet<String> =
        nodes.iter().map(|n| n.session_id.clone()).collect();
    let mut children: HashMap<Option<String>, Vec<TreeNode>> = HashMap::new();
    for n in nodes {
        let parent = n
            .parent
            .as_ref()
            .map(|(p, _)| p.clone())
            .filter(|p| ids.contains(p));
        children.entry(parent).or_default().push(n);
    }
    let mut out = Vec::new();
    let mut stack: Vec<TreeNode> = children.remove(&None).unwrap_or_default();
    stack.reverse();
    while let Some(n) = stack.pop() {
        if let Some(mut kids) = children.remove(&Some(n.session_id.clone())) {
            kids.reverse();
            stack.extend(kids);
        }
        out.push(n);
    }
    out
}

/// One line per node of [`build`]'s order, indented two spaces per depth:
/// `* <label or id8>  step N · <origin> · $cost · <last prompt, 40 chars>`, with `*`
/// marking the current session (a space otherwise). Label and prompt are escaped.
pub fn lines(nodes: &[TreeNode]) -> Vec<String> {
    let mut depth: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        let d = n
            .parent
            .as_ref()
            .and_then(|(p, _)| depth.get(p.as_str()))
            .map_or(0, |d| d + 1);
        depth.insert(&n.session_id, d);
        let name = match &n.label {
            Some(l) => crate::printable(l),
            None => crate::printable(&n.session_id.chars().take(8).collect::<String>()),
        };
        let origin = serde_json::to_value(n.origin)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let cost = n
            .cost_usd
            .map_or_else(|| "$-".to_string(), |c| format!("${c:.4}"));
        let prompt: String = n.last_prompt.chars().take(PROMPT_SHOWN).collect();
        out.push(format!(
            "{}{} {name}  step {} · {origin} · {cost} · {}",
            "  ".repeat(d),
            if n.current { '*' } else { ' ' },
            n.last_step,
            crate::printable(&prompt)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: &str, parent: Option<&str>, created: u64) -> TreeNode {
        TreeNode {
            session_id: id.into(),
            parent: parent.map(|p| (p.to_string(), 1)),
            label: None,
            origin: Origin::User,
            created,
            last_step: 1,
            last_prompt: String::new(),
            cost_usd: None,
            live: false,
            current: false,
        }
    }

    #[test]
    fn branches_follow_their_parent_and_orphans_are_roots() {
        let got = order(vec![
            n("c-child-of-a", Some("a"), 3),
            n("b", None, 2),
            n("a", None, 1),
            n("d-orphan", Some("gone"), 4),
            n("e-child-of-c", Some("c-child-of-a"), 5),
        ]);
        let ids: Vec<&str> = got.iter().map(|n| n.session_id.as_str()).collect();
        assert_eq!(ids, ["a", "c-child-of-a", "e-child-of-c", "b", "d-orphan"]);
        let lines = lines(&got);
        assert!(lines[2].starts_with("      e-child-"), "{lines:?}");
        assert!(
            lines[3].starts_with("  b  step 1 · user · $- · "),
            "{lines:?}"
        );
    }

    #[test]
    fn labels_and_prompts_are_escaped() {
        let mut node = n("01ABCDEFGHIJ", None, 1);
        node.label = Some("x\x1b[31m".into());
        node.last_prompt = "fix\x07 it".into();
        node.current = true;
        let line = &lines(&[node])[0];
        assert!(!line.contains('\x1b') && !line.contains('\x07'), "{line:?}");
        assert!(line.starts_with("* x"), "{line:?}");
    }
}
