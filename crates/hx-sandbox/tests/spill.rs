//! The spill store: content-addressed writes that are skipped when the content is already
//! there, and age/size cleanup (checkpoint 2, I7).

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use hx_sandbox::{LocalSandbox, Sandbox, SpillCleanup, cleanup_spills};

fn sandbox() -> (tempfile::TempDir, LocalSandbox) {
    let t = tempfile::tempdir().unwrap();
    let sb = LocalSandbox::new(t.path().to_path_buf()).unwrap();
    (t, sb)
}

#[tokio::test]
async fn spilling_content_that_is_already_there_writes_nothing() {
    let (t, sb) = sandbox();
    let rel = sb.spill(b"same bytes").await.unwrap();
    let first = std::fs::metadata(t.path().join(&rel)).unwrap();
    let again = sb.spill(b"same bytes").await.unwrap();
    assert_eq!(again, rel);
    let second = std::fs::metadata(t.path().join(&rel)).unwrap();
    // A rewrite (temp file + rename) would leave a new inode behind.
    assert_eq!(first.ino(), second.ino());
}

#[tokio::test]
async fn a_truncated_spill_file_is_rewritten() {
    let (t, sb) = sandbox();
    let rel = sb.spill(b"0123456789").await.unwrap();
    std::fs::write(t.path().join(&rel), b"0123").unwrap();
    sb.spill(b"0123456789").await.unwrap();
    assert_eq!(std::fs::read(t.path().join(&rel)).unwrap(), b"0123456789");
}

#[tokio::test]
async fn spill_recreates_its_directory_and_gitignore_when_deleted() {
    let (t, sb) = sandbox();
    sb.spill(b"one").await.unwrap();
    std::fs::remove_dir_all(t.path().join(".hx/tmp")).unwrap();
    let rel = sb.spill(b"two").await.unwrap();
    assert_eq!(std::fs::read(t.path().join(&rel)).unwrap(), b"two");
    assert_eq!(
        std::fs::read_to_string(t.path().join(".hx/tmp/.gitignore")).unwrap(),
        "*"
    );
}

/// A spill file of `len` bytes whose mtime is `age` ago.
async fn aged(sb: &LocalSandbox, root: &Path, tag: u8, len: usize, age: Duration) {
    let rel = sb.spill(&vec![tag; len]).await.unwrap();
    let f = std::fs::File::options()
        .write(true)
        .open(root.join(rel))
        .unwrap();
    f.set_modified(SystemTime::now() - age).unwrap();
}

fn spill_count(root: &Path) -> usize {
    std::fs::read_dir(root.join(".hx/tmp/spill"))
        .unwrap()
        .count()
}

#[tokio::test]
async fn cleanup_removes_old_spills_then_the_oldest_beyond_the_cap() {
    let (t, sb) = sandbox();
    let day = Duration::from_secs(86_400);
    aged(&sb, t.path(), b'a', 100, day * 30).await; // too old
    aged(&sb, t.path(), b'b', 100, day * 3).await; // oldest within age
    aged(&sb, t.path(), b'c', 100, day * 2).await;
    aged(&sb, t.path(), b'd', 100, day).await;
    let got = cleanup_spills(t.path(), day * 7, 250).unwrap();
    assert_eq!(
        got,
        SpillCleanup {
            removed: 2,
            freed_bytes: 200,
            kept: 2,
            kept_bytes: 200,
        }
    );
    assert_eq!(spill_count(t.path()), 2);
    // The newest two survive.
    assert!(
        t.path()
            .join(sb.spill(&[b'c'; 100]).await.unwrap())
            .exists()
    );
    assert!(
        t.path()
            .join(sb.spill(&[b'd'; 100]).await.unwrap())
            .exists()
    );
}

#[tokio::test]
async fn cleanup_without_a_spill_store_does_nothing() {
    let (t, _sb) = sandbox();
    assert_eq!(
        cleanup_spills(t.path(), Duration::ZERO, 0).unwrap(),
        SpillCleanup::default()
    );
}

#[tokio::test]
async fn cleanup_never_follows_a_symlinked_store() {
    let (t, _sb) = sandbox();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("keep.txt"), b"precious").unwrap();
    std::fs::create_dir_all(t.path().join(".hx/tmp")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), t.path().join(".hx/tmp/spill")).unwrap();
    let got = cleanup_spills(t.path(), Duration::ZERO, 0).unwrap();
    assert_eq!(got.removed, 0);
    assert!(elsewhere.path().join("keep.txt").exists());
}
