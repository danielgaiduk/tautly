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

#[tokio::test]
async fn write_atomic_new_file_gets_umask_default_mode() {
    use std::os::unix::fs::MetadataExt;
    let (t, sb) = sandbox();
    sb.write_atomic(Path::new("plain.txt"), b"x", None)
        .await
        .unwrap();
    // Reference: a file created by the kernel under the same umask.
    let reference = t.path().join("ref.txt");
    std::fs::File::create(&reference).unwrap();
    let want = std::fs::metadata(&reference).unwrap().mode() & 0o777;
    let got = sb.metadata(Path::new("plain.txt")).await.unwrap().mode & 0o777;
    assert_eq!(got, want);
    assert_ne!(got, 0o600, "should not be owner-only unless umask says so");
}

#[tokio::test]
async fn walker_with_follow_links_does_not_leave_root() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
    let (t, sb) = sandbox();
    std::fs::write(t.path().join("inside.txt"), "i").unwrap();
    std::os::unix::fs::symlink(outside.path(), t.path().join("out")).unwrap();
    let root = t.path().canonicalize().unwrap();
    let names: Vec<_> = sb
        .walker(Path::new("."))
        .unwrap()
        .follow_links(true)
        .build()
        .filter_map(Result::ok)
        .map(|e| e.path().to_path_buf())
        .collect();
    assert!(names.iter().any(|p| p.ends_with("inside.txt")));
    assert!(
        !names.iter().any(|p| p.ends_with("secret.txt")),
        "{names:?}"
    );
    assert!(names.iter().all(|p| p.starts_with(&root)));
}

#[tokio::test]
async fn spill_is_content_addressed_relative_and_gitignored() {
    let (t, sb) = sandbox();
    let rel = sb.spill(b"hello spill").await.unwrap();
    assert!(rel.is_relative(), "{rel:?}");
    let hash = blake3::hash(b"hello spill").to_hex();
    assert_eq!(
        rel,
        Path::new(".hx/tmp/spill").join(format!("{}.txt", &hash[..16]))
    );
    assert_eq!(sb.read(&rel).await.unwrap(), b"hello spill");
    let gi = std::fs::read_to_string(t.path().join(".hx/tmp/.gitignore")).unwrap();
    assert_eq!(gi, "*");
    // Same content -> same path; different content -> different path.
    assert_eq!(sb.spill(b"hello spill").await.unwrap(), rel);
    assert_ne!(sb.spill(b"other").await.unwrap(), rel);
}

#[tokio::test]
async fn spill_keeps_existing_gitignore() {
    let (t, sb) = sandbox();
    std::fs::create_dir_all(t.path().join(".hx/tmp")).unwrap();
    std::fs::write(t.path().join(".hx/tmp/.gitignore"), "custom\n").unwrap();
    sb.spill(b"x").await.unwrap();
    let gi = std::fs::read_to_string(t.path().join(".hx/tmp/.gitignore")).unwrap();
    assert_eq!(gi, "custom\n");
}
