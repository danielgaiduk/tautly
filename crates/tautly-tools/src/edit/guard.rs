//! Stale-read guard (spec 5.4 step 1).

use std::path::Path;

use tautly_sandbox::FileMeta;

use super::EditError;
use crate::ReadTracker;

/// Reject the edit unless the model has read this file and neither its mtime nor its
/// content hash has changed since.
pub fn check_fresh(
    tracker: &ReadTracker,
    path: &Path,
    meta: &FileMeta,
    current: &[u8],
) -> Result<(), EditError> {
    let (mtime, hash) = tracker.last(path).ok_or(EditError::NotRead)?;
    if mtime != meta.mtime || hash != blake3::hash(current) {
        return Err(EditError::Stale);
    }
    Ok(())
}
