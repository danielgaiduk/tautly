use std::path::Path;
use tautly_sandbox::{LocalSandbox, Sandbox, SandboxError};

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
    use tautly_sandbox::SpawnSpec;
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

/// Review N2: a bare program name is looked up on the child's `PATH` with the rule git
/// already followed (absolute entries outside the root only), so `PATH=bin:...` or an
/// empty entry cannot make a user-trusted MCP server run a program the repository ships.
#[tokio::test]
async fn spawn_never_runs_a_bare_name_from_the_work_tree() {
    use std::os::unix::fs::PermissionsExt;
    use tautly_sandbox::SpawnSpec;
    use tokio::io::AsyncReadExt;
    let (t, sb) = sandbox();
    let tools = tempfile::tempdir().unwrap();
    let script = |path: &Path, says: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\necho {says}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    script(&t.path().join("bin/mytool"), "REPO");
    script(&t.path().join("mytool"), "REPO");
    script(&tools.path().join("mytool"), "OUTSIDE");
    let run = |path: String| {
        let sb = sb.clone();
        async move {
            let mut child = sb
                .spawn(SpawnSpec {
                    argv: vec!["mytool".into()],
                    cwd: ".".into(),
                    env: vec![("PATH".into(), path)],
                    env_remove: vec![],
                    new_process_group: true,
                })
                .await?;
            let mut out = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut out)
                .await
                .unwrap();
            Ok::<_, SandboxError>(out.trim().to_string())
        }
    };
    let o = tools.path().display();
    for path in [format!("bin:{o}"), format!(":{o}")] {
        assert_eq!(run(path.clone()).await.unwrap(), "OUTSIDE", "PATH={path}");
    }
    // Only the repository's: a clear error, nothing run.
    let err = run(format!("bin::{}", t.path().join("bin").display()))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("mytool not found on PATH outside the work tree"),
        "{err}"
    );
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
        Path::new(".tautly/tmp/spill").join(format!("{}.txt", &hash[..16]))
    );
    assert_eq!(sb.read(&rel).await.unwrap(), b"hello spill");
    let gi = std::fs::read_to_string(t.path().join(".tautly/tmp/.gitignore")).unwrap();
    assert_eq!(gi, "*");
    // Same content -> same path; different content -> different path.
    assert_eq!(sb.spill(b"hello spill").await.unwrap(), rel);
    assert_ne!(sb.spill(b"other").await.unwrap(), rel);
}

/// Review B-M1: a repository that commits its own `.tautly/tmp/.gitignore` (empty, or
/// with other rules) must not un-ignore the spill store, whose files hold full command
/// output: it is rewritten to `*`.
#[tokio::test]
async fn spill_rewrites_a_gitignore_that_is_not_ignore_all() {
    for committed in ["", "custom\n", "!*\n", "*\n!spill/\n"] {
        let (t, sb) = sandbox();
        std::fs::create_dir_all(t.path().join(".tautly/tmp")).unwrap();
        std::fs::write(t.path().join(".tautly/tmp/.gitignore"), committed).unwrap();
        sb.spill(b"x").await.unwrap();
        let gi = std::fs::read_to_string(t.path().join(".tautly/tmp/.gitignore")).unwrap();
        assert_eq!(gi, "*", "{committed:?}");
    }
}

/// A `.tautly/tmp/.gitignore` that is a symbolic link (to a file of the repository, or
/// dangling) is neither written through nor trusted: nothing is spilled.
#[tokio::test]
async fn spill_refuses_a_gitignore_that_is_a_link() {
    let (t, sb) = sandbox();
    std::fs::write(t.path().join("README.md"), "readme\n").unwrap();
    std::fs::create_dir_all(t.path().join(".tautly/tmp")).unwrap();
    std::os::unix::fs::symlink("../../README.md", t.path().join(".tautly/tmp/.gitignore")).unwrap();
    assert!(sb.spill(b"x").await.is_err());
    assert_eq!(
        std::fs::read_to_string(t.path().join("README.md")).unwrap(),
        "readme\n"
    );
    std::fs::remove_file(t.path().join(".tautly/tmp/.gitignore")).unwrap();
    std::os::unix::fs::symlink("../../NEW.md", t.path().join(".tautly/tmp/.gitignore")).unwrap();
    assert!(sb.spill(b"y").await.is_err());
    assert!(!t.path().join("NEW.md").exists());
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
    assert!(spilled.starts_with(".tautly/tmp"));
}

#[tokio::test]
async fn symlink_between_protected_paths_is_denied_too() {
    let (t, sb) = protected_repo();
    std::fs::create_dir_all(t.path().join(".tautly")).unwrap();
    // A repository-planted `.tautly/tmp -> ../.git/hooks` must not let spills land in hooks.
    std::os::unix::fs::symlink("../.git/hooks", t.path().join(".tautly/tmp")).unwrap();
    std::os::unix::fs::symlink("../.git/config", t.path().join(".tautly/notes")).unwrap();
    assert!(matches!(
        sb.write_atomic(Path::new(".tautly/notes"), b"x", None)
            .await,
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
///
/// The bound is absolute, not a ratio against `canonicalize`: a wall-clock ratio of two
/// ~10 ms loops flaked under load (one run in 24 measured 18x with a loaded machine).
/// 300 resolves take about 15 ms in a debug build, so 2 s is over 100x slack and still
/// far below any resolver that is quadratic or worse in the path (the structural guard is
/// `repeated_dotdot_over_missing_names_resolves_in_linear_time`).
#[tokio::test]
async fn resolve_of_a_deep_path_is_cheap() {
    let (t, sb) = sandbox();
    let rel: std::path::PathBuf = (0..30).map(|i| format!("d{i}")).collect();
    std::fs::create_dir_all(t.path().join(&rel)).unwrap();
    std::fs::write(t.path().join(&rel).join("f.txt"), b"x").unwrap();
    let file = rel.join("f.txt");
    let rounds = 300;
    // Warm the caches, then time.
    sb.resolve(&file).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        sb.resolve(&file).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "{rounds} resolves of a 30-deep path took {elapsed:?}"
    );
}

// --- `..` after a missing component must not skip the filesystem check ---
fn dotdot_repo() -> (tempfile::TempDir, tempfile::TempDir, LocalSandbox) {
    let (t, sb) = protected_repo();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), t.path().join("link_out")).unwrap();
    (t, outside, sb)
}

#[tokio::test]
async fn resolve_follows_symlink_reached_after_dotdot_over_missing_component() {
    let (t, outside, sb) = dotdot_repo();
    for p in [
        "missing/../link_out/x",
        "a/b/../../link_out/x/y",
        "missing/../link_out/../../escape",
    ] {
        assert!(
            matches!(sb.resolve(Path::new(p)), Err(SandboxError::OutsideRoot(_))),
            "{p}: {:?}",
            sb.resolve(Path::new(p))
        );
    }
    // The write that used to escape (it creates parent directories) is refused.
    let r = sb
        .write_atomic(Path::new("missing/../link_out/new/x.txt"), b"pwn", None)
        .await;
    assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{r:?}");
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    assert!(!t.path().join("missing").exists());
}

#[tokio::test]
async fn resolve_follows_symlink_after_dotdot_on_absolute_paths() {
    let (t, outside, sb) = dotdot_repo();
    let root = t.path().canonicalize().unwrap();
    let abs = root.join("missing/../link_out/x");
    assert!(matches!(
        sb.resolve(&abs),
        Err(SandboxError::OutsideRoot(_))
    ));
    let r = sb.write_atomic(&abs, b"pwn", None).await;
    assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{r:?}");
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    // An absolute path that lexically leaves the root through a missing component too.
    let out = root.join("missing/../../etc/passwd");
    assert!(matches!(
        sb.resolve(&out),
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn dotdot_over_missing_component_cannot_dodge_protected_symlink_targets() {
    let (t, _outside, sb) = dotdot_repo();
    // `docs/hooks -> ../.git/hooks`: reached after a lexical `..`, it is still a link into .git.
    for p in ["missing/../docs/hooks/pre-commit", "missing/../NOTES.md"] {
        let r = sb.write_atomic(Path::new(p), b"evil", None).await;
        assert!(matches!(r, Err(SandboxError::Denied(_))), "{p}: {r:?}");
    }
    assert!(!t.path().join(".git/hooks/pre-commit").exists());
    assert_eq!(
        std::fs::read_to_string(t.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
    // Naming the protected path itself (through a `..`) resolves to it, for the policy to ask about.
    let root = t.path().canonicalize().unwrap();
    assert_eq!(
        sb.resolve(Path::new("nope/../.git/hooks/pre-commit"))
            .unwrap(),
        root.join(".git/hooks/pre-commit")
    );
}

#[tokio::test]
async fn dotdot_inside_an_all_missing_tail_still_resolves_inside_root() {
    let (t, _outside, sb) = dotdot_repo();
    let root = t.path().canonicalize().unwrap();
    assert_eq!(
        sb.resolve(Path::new("a/b/../c.rs")).unwrap(),
        root.join("a/c.rs")
    );
    assert_eq!(
        sb.resolve(Path::new("src/new_dir/../new_file.rs")).unwrap(),
        root.join("src/new_file.rs")
    );
    assert_eq!(
        sb.resolve(&root.join("a/b/../c.rs")).unwrap(),
        root.join("a/c.rs")
    );
    // Over an existing directory, too.
    assert_eq!(
        sb.resolve(Path::new("docs/x/../y.md")).unwrap(),
        root.join("docs/y.md")
    );
    sb.write_atomic(Path::new("a/b/../c.rs"), b"ok", None)
        .await
        .unwrap();
    assert_eq!(std::fs::read(root.join("a/c.rs")).unwrap(), b"ok");
}

// --- one resolver for policy and sandbox (review round 1) ---
fn policy_for(root: &Path, allow: &[&str]) -> tautly_sandbox::Policy {
    tautly_sandbox::Policy::new(
        tautly_sandbox::PolicyConfig {
            mode: tautly_sandbox::Mode::Ask,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: vec![],
        },
        root.to_path_buf(),
    )
    .unwrap()
}

fn write_effect(p: &Path) -> tautly_sandbox::Effect {
    tautly_sandbox::Effect::Write {
        paths: vec![p.to_path_buf()],
    }
}

/// `src/a -> ../other`, `other/lnk -> ../src/deep`: `src/a/lnk/..` is `src`.
fn two_link_repo() -> (tempfile::TempDir, LocalSandbox) {
    let (t, sb) = protected_repo();
    for d in ["src/deep", "other"] {
        std::fs::create_dir_all(t.path().join(d)).unwrap();
    }
    std::os::unix::fs::symlink("../other", t.path().join("src/a")).unwrap();
    std::os::unix::fs::symlink("../src/deep", t.path().join("other/lnk")).unwrap();
    (t, sb)
}

#[tokio::test]
async fn policy_and_sandbox_agree_on_dotdot_over_links_after_a_missing_component() {
    let (t, sb) = two_link_repo();
    let root = sb.root().to_path_buf();
    let policy = policy_for(&root, &["write(src/**)"]);
    let req = Path::new("src/missing/../a/lnk/../b");
    // The kernel (and the policy's link walk) reads this as src/b, inside the allow rule.
    assert_eq!(
        policy.decide("write", &write_effect(req), None),
        tautly_sandbox::Decision::Allow
    );
    assert_eq!(sb.resolve(req).unwrap(), root.join("src/b"));
    sb.write_atomic(req, b"x", None).await.unwrap();
    assert!(t.path().join("src/b").exists());
    assert!(!t.path().join("other/b").exists());
}

#[tokio::test]
async fn policy_and_sandbox_agree_when_dotdot_after_a_missing_component_leaves_a_protected_link() {
    let (t, sb) = two_link_repo();
    let root = sb.root().to_path_buf();
    // docs/hooks -> ../.git/hooks, so docs/hooks/.. is .git: this names .git/config.
    let req = Path::new("missing/../docs/hooks/../config");
    assert!(matches!(
        policy_for(&root, &["write"]).decide("write", &write_effect(req), None),
        tautly_sandbox::Decision::Ask { .. }
    ));
    assert_eq!(sb.resolve(req).unwrap(), root.join(".git/config"));
    let r = sb.write_atomic(req, b"evil", None).await;
    assert!(matches!(r, Err(SandboxError::Denied(_))), "{r:?}");
    assert!(!t.path().join("docs/config").exists());
    assert_eq!(
        std::fs::read_to_string(t.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[tokio::test]
async fn dangling_symlinks_are_followed_by_resolve() {
    let (t, _outside, sb) = dotdot_repo();
    let outside = t.path().parent().unwrap().join("tautly-no-such-target");
    std::os::unix::fs::symlink(&outside, t.path().join("dangle_out")).unwrap();
    std::os::unix::fs::symlink("made/later.txt", t.path().join("dangle_in")).unwrap();
    let root = sb.root().to_path_buf();
    for p in ["dangle_out", "missing/../dangle_out", "dangle_out/sub/x"] {
        let r = sb.resolve(Path::new(p));
        assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{p}: {r:?}");
    }
    assert!(!outside.exists());
    for p in ["dangle_in", "missing/../dangle_in"] {
        assert_eq!(
            sb.resolve(Path::new(p)).unwrap(),
            root.join("made/later.txt")
        );
    }
    sb.write_atomic(Path::new("missing/../dangle_in"), b"x", None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(t.path().join("made/later.txt")).unwrap(),
        b"x"
    );
}

#[tokio::test]
async fn symlink_loop_after_dotdot_is_an_error_not_a_hang() {
    let (t, sb) = sandbox();
    std::os::unix::fs::symlink("loop2", t.path().join("loop1")).unwrap();
    std::os::unix::fs::symlink("loop1", t.path().join("loop2")).unwrap();
    for p in ["loop1/x", "missing/../loop1/x", "missing/../loop1"] {
        assert!(sb.resolve(Path::new(p)).is_err(), "{p}");
    }
}

#[tokio::test]
async fn trailing_dotdot_after_missing_and_after_an_outside_link() {
    let (t, _outside, sb) = dotdot_repo();
    let root = sb.root().to_path_buf();
    assert_eq!(sb.resolve(Path::new("missing/..")).unwrap(), root);
    assert_eq!(sb.resolve(Path::new("a/b/../..")).unwrap(), root);
    assert_eq!(
        sb.resolve(Path::new("docs/x/..")).unwrap(),
        root.join("docs")
    );
    // `link_out/..` is the outside directory's parent, not the root.
    for p in ["missing/../link_out/..", "missing/../link_out/../x"] {
        let r = sb.resolve(Path::new(p));
        assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{p}: {r:?}");
    }
    drop(t);
}

#[tokio::test]
async fn resolve_outside_the_root_never_reveals_what_exists() {
    let (t, sb) = sandbox();
    let deep = "../".repeat(30);
    for p in [
        format!("missing/{deep}etc/passwd/x"),
        format!("missing/{deep}etc/passwd"),
        format!("missing/{deep}etc/no-such-dir/x"),
        format!("{deep}etc/passwd/x"),
    ] {
        let r = sb.resolve(Path::new(&p));
        assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{p}: {r:?}");
    }
    let r = sb.resolve(Path::new("/etc/passwd/x"));
    assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{r:?}");
    // Inside the root a file used as a directory is still reported as such.
    std::fs::write(t.path().join("f.txt"), "x").unwrap();
    assert!(matches!(
        sb.resolve(Path::new("f.txt/x")),
        Err(SandboxError::Io(_))
    ));
}

// --- fix round 2: linear walk, remove_file acts on the link, minors ---

/// Run `f` on a thread and fail if it takes longer than `limit` (so a quadratic
/// resolver fails fast instead of hanging the suite).
fn within<T: Send + 'static>(
    limit: std::time::Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("did not finish within {limit:?}"))
}

#[tokio::test]
async fn repeated_dotdot_over_missing_names_resolves_in_linear_time() {
    let (t, sb) = sandbox();
    std::fs::write(t.path().join("f.txt"), "x").unwrap();
    let root = sb.root().to_path_buf();
    let path = format!("{}f.txt", "m/../".repeat(3000));
    let limit = std::time::Duration::from_secs(2);
    let (sb2, p2) = (sb.clone(), path.clone());
    let got = within(limit, move || sb2.resolve(Path::new(&p2)));
    assert_eq!(got.unwrap(), root.join("f.txt"));
    // The policy walks the same path twice per decision, before any prompt.
    let policy = policy_for(&root, &["write"]);
    let eff = write_effect(Path::new(&path));
    let decision = within(limit, move || policy.decide("write", &eff, None));
    assert_eq!(decision, tautly_sandbox::Decision::Allow);
    // Growing the input tenfold must not grow the cost by more than a small factor.
    let long = format!("{}f.txt", "m/../".repeat(30000));
    let start = std::time::Instant::now();
    let r = within(limit, move || sb.resolve(Path::new(&long)));
    assert_eq!(r.unwrap(), root.join("f.txt"));
    assert!(start.elapsed() < limit);
}

/// A path to a file that does not exist yet costs a few filesystem calls, not one per
/// missing component. Absolute bound for the same reason as above: 300 resolves take about
/// 11 ms in a debug build.
#[tokio::test]
async fn new_file_paths_are_cheap_to_resolve() {
    let (t, sb) = sandbox();
    let dir: std::path::PathBuf = (0..5).map(|i| format!("d{i}")).collect();
    std::fs::create_dir_all(t.path().join(&dir)).unwrap();
    let new_file = dir.join("a/b/new.rs");
    let rounds = 300;
    sb.resolve(&new_file).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        sb.resolve(&new_file).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "{rounds} resolves of a new-file path took {elapsed:?}"
    );
}

#[tokio::test]
async fn remove_file_removes_a_dangling_link_itself() {
    let (t, sb) = sandbox();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim.txt"), b"keep").unwrap();
    std::os::unix::fs::symlink("no/such/target", t.path().join("dangling")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("nope"), t.path().join("dangling_out")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("victim.txt"),
        t.path().join("to_victim"),
    )
    .unwrap();
    for name in ["dangling", "dangling_out", "to_victim"] {
        sb.remove_file(Path::new(name)).await.unwrap();
        assert!(
            std::fs::symlink_metadata(t.path().join(name)).is_err(),
            "{name} still there"
        );
    }
    // A link to a file outside the root is only unlinked, never followed.
    assert_eq!(
        std::fs::read(outside.path().join("victim.txt")).unwrap(),
        b"keep"
    );
    // Missing names stay an error, and the parent is still confined.
    assert!(matches!(
        sb.remove_file(Path::new("dangling")).await,
        Err(SandboxError::Io(_))
    ));
    assert!(matches!(
        sb.remove_file(Path::new("missing/../../x")).await,
        Err(SandboxError::OutsideRoot(_))
    ));
}

#[tokio::test]
async fn remove_file_still_refuses_links_into_protected_paths() {
    let (t, sb) = protected_repo();
    // A link into .git stays undeletable (as before), and its target is untouched.
    let r = sb.remove_file(Path::new("NOTES.md")).await;
    assert!(matches!(r, Err(SandboxError::Denied(_))), "{r:?}");
    assert!(std::fs::symlink_metadata(t.path().join("NOTES.md")).is_ok());
    assert_eq!(
        std::fs::read_to_string(t.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
    // Through a directory link into .git the parent itself is protected.
    std::fs::write(t.path().join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    let r = sb.remove_file(Path::new("docs/hooks/pre-commit")).await;
    assert!(matches!(r, Err(SandboxError::Denied(_))), "{r:?}");
    assert!(t.path().join(".git/hooks/pre-commit").exists());
}

#[tokio::test]
async fn symlink_loop_outside_the_root_is_outside_not_eloop() {
    let (_t, outside, sb) = dotdot_repo();
    std::os::unix::fs::symlink("b", outside.path().join("a")).unwrap();
    std::os::unix::fs::symlink("a", outside.path().join("b")).unwrap();
    for p in ["link_out/a/x", "link_out/a", "missing/../link_out/b/x"] {
        let r = sb.resolve(Path::new(p));
        assert!(matches!(r, Err(SandboxError::OutsideRoot(_))), "{p}: {r:?}");
    }
}

#[tokio::test]
async fn case_only_difference_is_not_called_a_symlink() {
    let (t, sb) = protected_repo();
    let root = sb.root().to_path_buf();
    let policy = policy_for(&root, &["write"]);
    let tautly_sandbox::Decision::Ask { reason } =
        policy.decide("write", &write_effect(Path::new(".GIT/config")), None)
    else {
        panic!("expected ask");
    };
    assert!(!reason.contains("symlink"), "{reason}");
    // A real link is still named.
    let tautly_sandbox::Decision::Ask { reason } =
        policy.decide("write", &write_effect(Path::new("NOTES.md")), None)
    else {
        panic!("expected ask");
    };
    assert!(reason.contains(".git/config"), "{reason}");
    drop(t);
}
