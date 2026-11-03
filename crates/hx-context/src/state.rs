//! Structured working state: task, plan, files touched, decisions, questions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hx_tools::{AppliedEffect, PlanStep, StatePatch, StepStatus};

use crate::tokens::TokenEstimator;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNote {
    /// Hex blake3 hash of the file as last read, or as written by the last edit.
    pub read_hash: Option<String>,
    pub edited: bool,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingState {
    pub task: Option<String>,
    pub plan: Vec<PlanStep>,
    pub files: BTreeMap<PathBuf, FileNote>,
    pub decisions: Vec<String>,
    pub questions: Vec<String>,
    /// At most 10 are rendered, within 300 tokens; set by the engine.
    pub failed_attempts: Vec<String>,
}

/// Failed attempts shown at most, newest kept.
const MAX_FAILED_ATTEMPTS: usize = 10;
/// Token cap for the rendered failed-attempts list.
const FAILED_ATTEMPTS_TOKENS: u32 = 300;
/// Hex characters of a file hash shown in the state block.
const HASH_SHOWN: usize = 12;

impl WorkingState {
    /// Record tool side effects on files. `Exec` is ignored: a command may have touched any
    /// file, but nothing is known about which.
    pub fn apply_effects(&mut self, effects: &[AppliedEffect]) {
        for effect in effects {
            match effect {
                AppliedEffect::Read {
                    path, file_hash, ..
                } => {
                    let note = self.files.entry(path.clone()).or_default();
                    note.read_hash = Some(file_hash.to_hex().to_string());
                    note.deleted = false;
                }
                AppliedEffect::Edited {
                    path, post_hash, ..
                } => {
                    let note = self.files.entry(path.clone()).or_default();
                    note.read_hash = Some(post_hash.to_hex().to_string());
                    note.edited = true;
                    note.deleted = false;
                }
                AppliedEffect::Deleted { path, .. } => {
                    let note = self.files.entry(path.clone()).or_default();
                    note.read_hash = None;
                    note.edited = false;
                    note.deleted = true;
                }
                AppliedEffect::Exec { .. } => {}
            }
        }
    }

    pub fn apply_patch(&mut self, patch: StatePatch) {
        if let Some(plan) = patch.plan {
            self.plan = plan;
        }
        for d in patch.decisions_add {
            if !self.decisions.contains(&d) {
                self.decisions.push(d);
            }
        }
        for q in patch.questions_add {
            if !self.questions.contains(&q) {
                self.questions.push(q);
            }
        }
        // Resolve against the list as the model last saw it: collect, then remove from the back.
        let mut resolved = patch.questions_resolve;
        resolved.sort_unstable();
        resolved.dedup();
        for i in resolved.into_iter().rev() {
            if i < self.questions.len() {
                self.questions.remove(i);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.task.is_none()
            && self.plan.is_empty()
            && self.files.is_empty()
            && self.decisions.is_empty()
            && self.questions.is_empty()
            && self.failed_attempts.is_empty()
    }

    /// The `<working_state>` block, or None when the state is empty. File paths are shown
    /// relative to `root` when they are inside it.
    pub fn render(&self, root: Option<&Path>, est: &TokenEstimator) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::from("<working_state>\n");
        if let Some(task) = &self.task {
            out.push_str(&format!("task: {task}\n"));
        }
        if !self.plan.is_empty() {
            out.push_str("plan:\n");
            for (i, step) in self.plan.iter().enumerate() {
                let status = match step.status {
                    StepStatus::Pending => "pending",
                    StepStatus::InProgress => "in progress",
                    StepStatus::Done => "done",
                };
                out.push_str(&format!("{}. [{status}] {}\n", i + 1, step.text));
            }
        }
        if !self.files.is_empty() {
            out.push_str("files:\n");
            for (path, note) in &self.files {
                let shown = root
                    .and_then(|r| path.strip_prefix(r).ok())
                    .unwrap_or(path)
                    .display();
                let mut tags = Vec::new();
                if note.deleted {
                    tags.push("deleted".to_string());
                }
                if let Some(h) = &note.read_hash {
                    let h = &h[..h.len().min(HASH_SHOWN)];
                    tags.push(format!("hash {h}"));
                }
                if note.edited {
                    tags.push("edited".into());
                }
                out.push_str(&format!("- {shown} ({})\n", tags.join(", ")));
            }
        }
        if !self.decisions.is_empty() {
            out.push_str("decisions:\n");
            for d in &self.decisions {
                out.push_str(&format!("- {d}\n"));
            }
        }
        if !self.questions.is_empty() {
            out.push_str("open questions:\n");
            for (i, q) in self.questions.iter().enumerate() {
                out.push_str(&format!("[{i}] {q}\n"));
            }
        }
        let failed = self.render_failed_attempts(est);
        if !failed.is_empty() {
            out.push_str("failed attempts (do not repeat):\n");
            for f in failed {
                out.push_str(&format!("- {f}\n"));
            }
        }
        out.push_str("</working_state>");
        Some(out)
    }

    /// The newest `MAX_FAILED_ATTEMPTS` attempts, oldest first, trimmed from the old end so
    /// the list fits `FAILED_ATTEMPTS_TOKENS` (a single oversized entry is cut short).
    fn render_failed_attempts(&self, est: &TokenEstimator) -> Vec<String> {
        let start = self
            .failed_attempts
            .len()
            .saturating_sub(MAX_FAILED_ATTEMPTS);
        let mut left = FAILED_ATTEMPTS_TOKENS;
        let mut kept = Vec::new();
        for attempt in self.failed_attempts[start..].iter().rev() {
            let attempt = attempt.trim();
            let cost = est.estimate(attempt).max(1);
            if cost <= left {
                left -= cost;
                kept.push(attempt.to_string());
                continue;
            }
            if kept.is_empty() {
                kept.push(truncate_to_tokens(attempt, left, est));
            }
            break;
        }
        kept.reverse();
        kept
    }
}

/// `s` cut at a char boundary so its estimate stays within `tokens`.
fn truncate_to_tokens(s: &str, tokens: u32, est: &TokenEstimator) -> String {
    let mut end = s.len();
    while end > 0 && est.estimate(&s[..end]) > tokens {
        end -= 1;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
    }
    s[..end].to_string()
}
