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
            env_remove: vec![],
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

#[tokio::test]
async fn remove_file_deletes_and_errors_when_missing() {
    let (t, sb) = sandbox();
    sb.write_atomic(Path::new("d/gone.txt"), b"x", None)
        .await
        .unwrap();
    sb.remove_file(Path::new("d/gone.txt")).await.unwrap();
    assert!(!t.path().join("d/gone.txt").exists());
    match sb.remove_file(Path::new("d/gone.txt")).await {
        Err(SandboxError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn remove_file_rejects_escape_and_symlink_outside() {
    let (t, sb) = sandbox();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim.txt"), b"keep").unwrap();
    assert!(matches!(
        sb.remove_file(Path::new("../victim.txt")).await,
        Err(SandboxError::OutsideRoot(_))
    ));
    std::os::unix::fs::symlink(outside.path(), t.path().join("link")).unwrap();
    assert!(matches!(
        sb.remove_file(Path::new("link/victim.txt")).await,
        Err(SandboxError::OutsideRoot(_))
    ));
    assert!(outside.path().join("victim.txt").exists());
}

// --- checkpoint fix: C1 defense in depth ---
fn protected_repo() -> (tempfile::TempDir, LocalSandbox) {
    let (t, sb) = sandbox();
    std::fs::create_dir_all(t.path().join(".git/hooks")).unwrap();
    std::fs::create_dir_all(t.path().join("docs")).unwrap();
    std::fs::write(t.path().join(".git/config"), "[core]\n").unwrap();
    std::fs::write(t.path().join("a.txt"), "a").unwrap();
    std::os::unix::fs::symlink(".git/config", t.path().join("NOTES.md")).unwrap();
    std::os::unix::fs::symlink("../.git/hooks", t.path().join("docs/hooks")).unwrap();
    std::os::unix::fs::symlink("a.txt", t.path().join("link.txt")).unwrap();
    (t, sb)
}

#[tokio::test]
async fn write_through_symlink_to_protected_path_is_denied() {
    let (t, sb) = protected_repo();
    for p in ["NOTES.md", "docs/hooks/pre-commit"] {
        let r = sb
            .write_atomic(Path::new(p), b"[core]\n\tfsmonitor = x\n", None)
            .await;
        assert!(matches!(r, Err(SandboxError::Denied(_))), "{p}: {r:?}");
    }
    let abs = t.path().join("NOTES.md");
    assert!(matches!(
        sb.write_atomic(&abs, b"x", None).await,
        Err(SandboxError::Denied(_))
    ));
    assert!(matches!(
        sb.remove_file(Path::new("NOTES.md")).await,
        Err(SandboxError::Denied(_))
    ));
    assert_eq!(
        std::fs::read_to_string(t.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
    assert!(!t.path().join(".git/hooks/pre-commit").exists());
}

#[tokio::test]
async fn explicit_protected_and_ordinary_symlink_writes_still_work() {
    let (t, sb) = protected_repo();
    // Naming the protected path itself is the policy's call (it asks), not a symlink trick.
    sb.write_atomic(Path::new(".git/config"), b"ok\n", None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join(".git/config")).unwrap(),
        "ok\n"
    );
    sb.write_atomic(Path::new("link.txt"), b"b", None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(t.path().join("a.txt")).unwrap(),
        "b"
    );
    let spilled = sb.spill(b"spill").await.unwrap();
    assert!(spilled.starts_with(".hx/tmp"));
}

#[tokio::test]
async fn symlink_between_protected_paths_is_denied_too() {
    let (t, sb) = protected_repo();
    std::fs::create_dir_all(t.path().join(".hx")).unwrap();
    // A repository-planted `.hx/tmp -> ../.git/hooks` must not let spills land in hooks.
    std::os::unix::fs::symlink("../.git/hooks", t.path().join(".hx/tmp")).unwrap();
    std::os::unix::fs::symlink("../.git/config", t.path().join(".hx/notes")).unwrap();
    assert!(matches!(
        sb.write_atomic(Path::new(".hx/notes"), b"x", None).await,
        Err(SandboxError::Denied(_))
    ));
    assert!(sb.spill(b"output").await.is_err());
    assert!(!t.path().join(".git/hooks/.gitignore").exists());
    assert_eq!(
        std::fs::read_to_string(t.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
    // Naming the protected path itself is still the policy's call.
    sb.write_atomic(Path::new(".git/config"), b"ok\n", None)
        .await
        .unwrap();
}

/// `resolve` canonicalizes once, not once per path component (checkpoint 2, I6): on a
/// deep path it costs about what one `canonicalize` does, never a multiple per level.
#[tokio::test]
async fn resolve_of_a_deep_path_costs_about_one_canonicalize() {
    let (t, sb) = sandbox();
    let rel: std::path::PathBuf = (0..30).map(|i| format!("d{i}")).collect();
    std::fs::create_dir_all(t.path().join(&rel)).unwrap();
    std::fs::write(t.path().join(&rel).join("f.txt"), b"x").unwrap();
    let file = rel.join("f.txt");
    let abs = t.path().join(&file);
    let rounds = 300;
    let time = |f: &dyn Fn()| {
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            f();
        }
        start.elapsed()
    };
    // Warm the caches, then compare.
    sb.resolve(&file).unwrap();
    let one = time(&|| {
        abs.canonicalize().unwrap();
    });
    let resolve = time(&|| {
        sb.resolve(&file).unwrap();
    });
    assert!(
        resolve < one * 4,
        "resolve {resolve:?} vs canonicalize {one:?} for {rounds} rounds"
    );
}
