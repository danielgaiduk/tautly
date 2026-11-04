//! The advisory lock on a work tree's index database (Task 31): an open workspace holds it
//! shared, `tautly index rebuild` takes it exclusively before deleting the database.

use std::time::Duration;

use tautly_core::{IndexLockError, Workspaces};
use tempfile::TempDir;

#[tokio::test]
async fn an_open_workspace_blocks_the_exclusive_lock_until_it_is_dropped() {
    let (data, work) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    std::fs::write(work.path().join("a.rs"), "fn a() {}\n").unwrap();
    let workspaces = Workspaces::new(data.path());
    let ws = workspaces.get(work.path()).unwrap();

    // Not open yet: nothing holds the lock.
    drop(ws.try_lock_index_exclusive().expect("free"));

    ws.start();
    assert!(ws.wait_ready(Duration::from_secs(60)).await);
    assert!(matches!(
        ws.try_lock_index_exclusive(),
        Err(IndexLockError::Busy)
    ));
    assert!(ws.index_db().exists());
    drop(ws);
    drop(workspaces);

    let ws = Workspaces::new(data.path()).get(work.path()).unwrap();
    let held = ws
        .try_lock_index_exclusive()
        .expect("released with the workspace");
    // Held: a second exclusive attempt refuses.
    assert!(matches!(
        ws.try_lock_index_exclusive(),
        Err(IndexLockError::Busy)
    ));
    drop(held);
}
