//! Pending tool approvals, keyed by request id (spec 3.4).
//!
//! An interactive client answers each `ToolApprovalRequested{request_id}` with
//! `Approve`/`Deny` naming that id. Without an interactive client (headless) nothing is
//! asked: `ask` resolves to deny at once. Pending requests are denied on `Shutdown` (or
//! when the client goes away), so nothing waits forever.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tautly_sandbox::{Effect, Mode, Policy, PolicyConfig};
use tautly_tools::{BashTool, EditTool, Tool, WebFetchTool, WriteTool};
use tokio::sync::oneshot;

/// The exact-target session rule a client offers for a tool call ("allow for this
/// session", spec 7.11.8): `edit(<path>)`, `write(<path>)`, `bash(<command>)`,
/// `web_fetch(<host>)` or `mcp__<server>__<tool>`, built with the permission policy's own
/// parser (quoted words, redirects and root-relative paths as rules see them) and glob
/// characters escaped, so the engine accepts it for this call. None when no single rule
/// names the call (several commands, a patch, a path outside `root`, parentheses). `root`
/// is where the session started (its work tree's top level is used, as the engine does).
/// Reads the file system (symlinks under the root): call it off the async workers if
/// that matters.
pub fn exact_rule(root: &Path, tool: &str, args: &Value) -> Option<String> {
    let effect = match tool {
        "bash" => BashTool.effect(args),
        "edit" => EditTool.effect(args),
        "write" => WriteTool.effect(args),
        "web_fetch" => WebFetchTool.effect(args),
        t if t.starts_with("mcp__") => Ok(Effect::Exec { argv: Vec::new() }),
        _ => return None,
    }
    .ok()?;
    let raw = args
        .get("command")
        .and_then(Value::as_str)
        .filter(|_| tool == "bash");
    let root = crate::session_root(root).unwrap_or_else(|_| root.to_path_buf());
    let cfg = PolicyConfig {
        mode: Mode::Ask,
        allow: Vec::new(),
        deny: Vec::new(),
    };
    Policy::new(cfg, root).ok()?.exact_rule(tool, &effect, raw)
}

/// Open requests: what each is for, and where its verdict goes.
type Pending = HashMap<String, (ApprovalFor, oneshot::Sender<Verdict>)>;

/// What an approval request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalFor {
    Tool,
    McpServer,
}

/// The answer to one approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Allowed; for a tool call, `session_rule` asks to allow calls it matches for the
    /// rest of the session (spec 7.11.8), which the turn checks against the call first.
    Approved {
        session_rule: Option<String>,
    },
    Denied {
        reason: Option<String>,
    },
}

/// Shared between the actor (which receives `Approve`/`Deny`) and the turn (which waits).
#[derive(Clone)]
pub struct Approvals {
    interactive: bool,
    pending: Arc<Mutex<Pending>>,
    next: Arc<AtomicU64>,
}

impl Approvals {
    pub fn new(interactive: bool) -> Self {
        Approvals {
            interactive,
            pending: Arc::default(),
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Whether a client can answer requests (otherwise [`Approvals::open`] gives None).
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Open a request: its id and where the verdict arrives. None in headless mode, where
    /// nobody can answer.
    pub fn open(&self, what: ApprovalFor) -> Option<(String, oneshot::Receiver<Verdict>)> {
        if !self.interactive {
            return None;
        }
        let id = format!("approval-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id.clone(), (what, tx));
        Some((id, rx))
    }

    /// What pending request `id` is for; None when no such request is pending.
    pub fn kind(&self, id: &str) -> Option<ApprovalFor> {
        self.lock().get(id).map(|(what, _)| *what)
    }

    /// Answer request `id`; false when no such request is pending.
    pub fn resolve(&self, id: &str, verdict: Verdict) -> bool {
        match self.lock().remove(id) {
            Some((_, tx)) => {
                let _ = tx.send(verdict);
                true
            }
            None => false,
        }
    }

    /// Forget request `id` without an answer (the waiting side gave up).
    pub fn withdraw(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Deny every pending request.
    pub fn deny_all(&self, reason: &str) {
        let pending: Vec<_> = self.lock().drain().collect();
        for (_, (_, tx)) in pending {
            let _ = tx.send(Verdict::Denied {
                reason: Some(reason.to_string()),
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_resolve_by_id_and_headless_never_opens_one() {
        assert!(Approvals::new(false).open(ApprovalFor::Tool).is_none());
        let a = Approvals::new(true);
        let (id1, rx1) = a.open(ApprovalFor::Tool).unwrap();
        let (id2, rx2) = a.open(ApprovalFor::McpServer).unwrap();
        assert_ne!(id1, id2);
        assert_eq!(a.kind(&id1), Some(ApprovalFor::Tool));
        assert_eq!(a.kind(&id2), Some(ApprovalFor::McpServer));
        assert_eq!(a.kind("nope"), None);
        assert!(!a.resolve("nope", Verdict::Approved { session_rule: None }));
        assert!(a.resolve(&id2, Verdict::Approved { session_rule: None }));
        assert_eq!(rx2.await.unwrap(), Verdict::Approved { session_rule: None });
        a.deny_all("shutting down");
        assert!(
            matches!(rx1.await.unwrap(), Verdict::Denied { reason: Some(r) } if r == "shutting down")
        );
        assert!(!a.resolve(&id1, Verdict::Approved { session_rule: None }));
    }
}
