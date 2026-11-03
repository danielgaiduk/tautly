//! Pending tool approvals, keyed by request id (spec 3.4).
//!
//! An interactive client answers each `ToolApprovalRequested{request_id}` with
//! `Approve`/`Deny` naming that id. Without an interactive client (headless) nothing is
//! asked: `ask` resolves to deny at once. Pending requests are denied on `Shutdown` (or
//! when the client goes away), so nothing waits forever.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

/// The answer to one approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    Denied { reason: Option<String> },
}

/// Shared between the actor (which receives `Approve`/`Deny`) and the turn (which waits).
#[derive(Clone)]
pub struct Approvals {
    interactive: bool,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Verdict>>>>,
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

    /// Whether a client can answer requests (else `ask` resolves to deny).
    pub fn interactive(&self) -> bool {
        self.interactive
    }

    /// Open a request: its id and where the verdict arrives. None in headless mode, where
    /// nobody can answer.
    pub fn open(&self) -> Option<(String, oneshot::Receiver<Verdict>)> {
        if !self.interactive {
            return None;
        }
        let id = format!("approval-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id.clone(), tx);
        Some((id, rx))
    }

    /// Answer request `id`; false when no such request is pending.
    pub fn resolve(&self, id: &str, verdict: Verdict) -> bool {
        match self.lock().remove(id) {
            Some(tx) => {
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
        for (_, tx) in pending {
            let _ = tx.send(Verdict::Denied {
                reason: Some(reason.to_string()),
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Verdict>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_resolve_by_id_and_headless_never_opens_one() {
        assert!(Approvals::new(false).open().is_none());
        let a = Approvals::new(true);
        let (id1, rx1) = a.open().unwrap();
        let (id2, rx2) = a.open().unwrap();
        assert_ne!(id1, id2);
        assert!(!a.resolve("nope", Verdict::Approved));
        assert!(a.resolve(&id2, Verdict::Approved));
        assert_eq!(rx2.await.unwrap(), Verdict::Approved);
        a.deny_all("shutting down");
        assert!(
            matches!(rx1.await.unwrap(), Verdict::Denied { reason: Some(r) } if r == "shutting down")
        );
        assert!(!a.resolve(&id1, Verdict::Approved));
    }
}
