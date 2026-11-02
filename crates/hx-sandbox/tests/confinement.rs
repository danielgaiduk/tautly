use hx_sandbox::{LocalSandbox, Sandbox, SandboxError};
use std::path::Path;

fn sandbox() -> (tempfile::TempDir, LocalSandbox) {
    let t = tempfile::tempdir().unwrap();
    let sb = LocalSandbox::new(t.path().to_path_buf()).unwrap();
    (t, sb)
}

#[tokio::test]
async fn resolve_rejects_parent_escape() {
    let (_t, sb) = sandbox();
    assert!(matches!(
        sb.resolve(Path::new("../etc/passwd")),
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn resolve_rejects_symlink_pointing_outside() {
    let (t, sb) = sandbox();
    std::os::unix::fs::symlink("/etc", t.path().join("link")).unwrap();
    assert!(matches!(
        sb.resolve(Path::new("link/passwd")),
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn resolve_rejects_absolute_outside() {
    let (_t, sb) = sandbox();
    assert!(matches!(
        sb.resolve(Path::new("/etc/passwd")),
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn resolve_allows_new_file_in_existing_dir() {
    let (t, sb) = sandbox();
    assert_eq!(
        sb.resolve(Path::new("new.txt")).unwrap(),
        t.path().canonicalize().unwrap().join("new.txt")
    );
}

#[tokio::test]
async fn resolve_allows_new_nested_tail() {
    let (t, sb) = sandbox();
    assert_eq!(
        sb.resolve(Path::new("a/b/c.txt")).unwrap(),
        t.path().canonicalize().unwrap().join("a/b/c.txt")
    );
}

#[tokio::test]
async fn write_atomic_then_read_roundtrip_preserves_mode() {
    let (_t, sb) = sandbox();
    let p = Path::new("script.sh");
    sb.write_atomic(p, b"#!/bin/sh\necho hi\n", Some(0o755))
        .await
        .unwrap();
    assert_eq!(sb.read(p).await.unwrap(), b"#!/bin/sh\necho hi\n");
    let meta = sb.metadata(p).await.unwrap();
    assert_eq!(meta.mode & 0o777, 0o755);
    assert_eq!(meta.len, 18);
}

#[tokio::test]
async fn write_atomic_creates_parent_dirs_and_rejects_escape() {
    let (_t, sb) = sandbox();
    sb.write_atomic(Path::new("x/y/z.txt"), b"ok", None)
        .await
        .unwrap();
    assert_eq!(sb.read(Path::new("x/y/z.txt")).await.unwrap(), b"ok");
    assert!(matches!(
        sb.write_atomic(Path::new("../evil"), b"no", None).await,
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn walker_survives_symlink_cycle() {
    let (t, sb) = sandbox();
    std::os::unix::fs::symlink(t.path(), t.path().join("loop")).unwrap();
    let n = sb
        .walker(Path::new("."))
        .unwrap()
        .follow_links(true)
        .build()
        .count();
    assert!(n < 100);
}

#[tokio::test]
async fn walker_rejects_base_outside_root() {
    let (_t, sb) = sandbox();
    assert!(matches!(
        sb.walker(Path::new("..")),
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn spawn_runs_in_cwd_with_env() {
    use hx_sandbox::SpawnSpec;
    use tokio::io::AsyncReadExt;
    let (_t, sb) = sandbox();
    let mut child = sb
        .spawn(SpawnSpec {
            argv: vec!["sh".into(), "-c".into(), "echo $FOO".into()],
            cwd: ".".into(),
            env: vec![("FOO".into(), "bar".into())],
            new_process_group: true,
        })
        .await
        .unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    assert_eq!(out.trim(), "bar");
}
