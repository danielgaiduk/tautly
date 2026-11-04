//! The approval key and store. The engine-level approval flow (events, headless,
//! shadowing, denial) is tested in `tautly-core/tests/mcp.rs`.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use tautly_mcp::approvals::key;
use tautly_mcp::{APPROVALS_FILE, ApprovalStore, ServerSpec};

fn stdio(command: &str, args: &[&str], env: &[(&str, &str)]) -> ServerSpec {
    ServerSpec::Stdio {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
    }
}

#[test]
fn approval_remembered_by_root_and_launch_hash() {
    let repo = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let spec = stdio("npx", &["-y", "server"], &[("TOKEN", "a")]);

    let k = key(repo.path(), "github", &spec);
    assert_eq!(k.len(), 64);
    assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    // Variable names and values are both part of the key (repository servers).
    assert_ne!(
        k,
        key(
            repo.path(),
            "github",
            &stdio("npx", &["-y", "server"], &[("TOKEN", "b")])
        )
    );
    assert_ne!(
        k,
        key(
            repo.path(),
            "github",
            &stdio("npx", &["-y", "server"], &[("OTHER", "a")])
        )
    );
    // Another repository or another name is another approval.
    assert_ne!(k, key(other.path(), "github", &spec));
    assert_ne!(k, key(repo.path(), "gitlab", &spec));
    // The root is canonical: a path through a symlink is the same repository.
    let link = data.path().join("link");
    std::os::unix::fs::symlink(repo.path(), &link).unwrap();
    assert_eq!(k, key(&link, "github", &spec));

    let path = data.path().join(APPROVALS_FILE);
    let mut store = ApprovalStore::open(&path);
    assert!(!store.is_approved(&k));
    store.approve(&k, repo.path(), "github").unwrap();
    assert!(store.is_approved(&k));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");

    // Read back by another session; a second approval keeps the first.
    let mut again = ApprovalStore::open(&path);
    assert!(again.is_approved(&k));
    let k2 = key(other.path(), "github", &spec);
    // Meanwhile a third process records one more.
    let mut third = ApprovalStore::open(&path);
    let k3 = key(repo.path(), "third", &spec);
    third.approve(&k3, repo.path(), "third").unwrap();
    again.approve(&k2, other.path(), "github").unwrap();
    let last = ApprovalStore::open(&path);
    assert!(last.is_approved(&k) && last.is_approved(&k2) && last.is_approved(&k3));
    // No temporary files are left next to it.
    let names: Vec<_> = std::fs::read_dir(data.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 3, "{names:?}"); // the file, its lock file and the symlink
}

#[test]
fn changed_command_asks_again() {
    let repo = tempfile::tempdir().unwrap();
    let base = stdio("node", &["server.js"], &[]);
    let k = key(repo.path(), "s", &base);
    let changed = [
        stdio("node", &["evil.js"], &[]),
        stdio("python", &["server.js"], &[]),
        stdio("node", &["server.js", "--x"], &[]),
        stdio(
            "node",
            &["server.js"],
            &[("NODE_OPTIONS", "--require ./x.js")],
        ),
        ServerSpec::Stdio {
            command: "node".into(),
            args: vec!["server.js".into()],
            env: BTreeMap::new(),
            cwd: Some(PathBuf::from("sub")),
        },
        ServerSpec::Http {
            url: "https://example.com/mcp".parse().unwrap(),
            headers: BTreeMap::new(),
        },
    ];
    for spec in &changed {
        assert_ne!(k, key(repo.path(), "s", spec), "{spec:?}");
    }
    // Argument boundaries matter: `a b` is not `a` + `b`.
    assert_ne!(
        key(repo.path(), "s", &stdio("sh", &["a b"], &[])),
        key(repo.path(), "s", &stdio("sh", &["a", "b"], &[]))
    );

    let http = |url: &str, headers: &[&str]| ServerSpec::Http {
        url: url.parse().unwrap(),
        headers: headers
            .iter()
            .map(|h| (h.to_string(), "secret".to_string()))
            .collect(),
    };
    let h = key(
        repo.path(),
        "s",
        &http("https://a.example/mcp", &["Authorization"]),
    );
    assert_ne!(
        h,
        key(
            repo.path(),
            "s",
            &http("https://b.example/mcp", &["Authorization"])
        )
    );
    assert_ne!(
        h,
        key(
            repo.path(),
            "s",
            &http("https://a.example/mcp", &["X-Other"])
        )
    );
    // The same headers, with the same values, are the same approval.
    assert_eq!(
        h,
        key(
            repo.path(),
            "s",
            &http("https://a.example/mcp", &["Authorization"])
        )
    );

    // A corrupt store is an empty one, and approving rewrites it.
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(APPROVALS_FILE);
    std::fs::write(&path, b"{not json").unwrap();
    let mut store = ApprovalStore::open(&path);
    assert!(!store.is_approved(&k));
    store.approve(&k, repo.path(), "s").unwrap();
    assert!(ApprovalStore::open(&path).is_approved(&k));
}

#[test]
fn changed_env_value_asks_again() {
    let repo = tempfile::tempdir().unwrap();
    let k = key(
        repo.path(),
        "s",
        &stdio("node", &["server.js"], &[("NODE_OPTIONS", "")]),
    );
    for (name, value) in [
        ("NODE_OPTIONS", "--require ./evil.js"),
        ("NODE_OPTIONS", " "),
    ] {
        let changed = stdio("node", &["server.js"], &[(name, value)]);
        assert_ne!(k, key(repo.path(), "s", &changed), "{name}={value}");
    }
    // Pairs, not a bag of strings: `A=b, B=a` is not `A=a, B=b`.
    assert_ne!(
        key(
            repo.path(),
            "s",
            &stdio("x", &[], &[("A", "b"), ("B", "a")])
        ),
        key(
            repo.path(),
            "s",
            &stdio("x", &[], &[("A", "a"), ("B", "b")])
        )
    );
    assert_eq!(
        key(repo.path(), "s", &stdio("x", &[], &[("PATH", "/bin")])),
        key(repo.path(), "s", &stdio("x", &[], &[("PATH", "/bin")]))
    );
}

#[test]
fn changed_header_value_asks_again() {
    let repo = tempfile::tempdir().unwrap();
    let http = |v: &str| ServerSpec::Http {
        url: "https://a.example/mcp".parse().unwrap(),
        headers: BTreeMap::from([("X-Target".to_string(), v.to_string())]),
    };
    assert_ne!(
        key(repo.path(), "s", &http("prod")),
        key(repo.path(), "s", &http("attacker"))
    );
}

#[test]
fn concurrent_approvals_are_all_kept() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(APPROVALS_FILE);
    let keys: Vec<String> = (0..8)
        .map(|i| key(repo.path(), &format!("s{i}"), &stdio("x", &[], &[])))
        .collect();
    std::thread::scope(|scope| {
        for (i, k) in keys.iter().enumerate() {
            let (path, root) = (path.clone(), repo.path().to_path_buf());
            scope.spawn(move || {
                // Each its own store, as two sessions (or two approval tasks) would have.
                let mut store = ApprovalStore::open(&path);
                store.approve(k, &root, &format!("s{i}")).unwrap();
            });
        }
    });
    let text = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).expect("the store is valid JSON");
    let store = ApprovalStore::open(&path);
    for k in &keys {
        assert!(store.is_approved(k), "lost {k}");
    }
    // Only the store and its lock file remain.
    let mut names: Vec<_> = std::fs::read_dir(data.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![APPROVALS_FILE.to_string(), format!("{APPROVALS_FILE}.lock")]
    );
}

/// Review B-I1: an approval covers the code the server runs. A word of the command line
/// (the command included) that names a file inside the root keys that file's content, so
/// a branch that rewrites an approved server's script asks again.
#[test]
fn rewritten_script_inside_the_root_asks_again() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    let write = |rel: &str, text: &str| {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    let k = |spec: &ServerSpec| key(&repo, "s", spec);
    let with_cwd = |cwd: &str, command: &str, args: &[&str]| ServerSpec::Stdio {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        cwd: Some(PathBuf::from(cwd)),
    };
    let outside = tempfile::tempdir().unwrap();
    let elsewhere = outside.path().canonicalize().unwrap().join("tool.js");
    std::fs::write(&elsewhere, "outside v1").unwrap();
    let elsewhere = elsewhere.to_str().unwrap().to_string();

    let cases: Vec<(&str, ServerSpec)> = vec![
        ("scripts/mcp.js", stdio("node", &["scripts/mcp.js"], &[])),
        ("run.sh", stdio("./run.sh", &["--stdio"], &[])),
        (
            "abs.js",
            stdio("node", &[repo.join("abs.js").to_str().unwrap()], &[]),
        ),
        (
            "pre.js",
            stdio("node", &["--require=./pre.js", "main.js"], &[]),
        ),
        ("sub/s.js", with_cwd("sub", "node", &["s.js"])),
        ("up.js", with_cwd("sub", "node", &["../up.js"])),
        ("target.js", stdio("node", &["link.js"], &[])),
    ];
    std::os::unix::fs::symlink(repo.join("target.js"), repo.join("link.js")).unwrap();
    for (file, spec) in &cases {
        write(file, "v1");
        let before = k(spec);
        assert_eq!(
            before,
            k(spec),
            "{file}: the same content is the same approval"
        );
        write(file, "v2: rewritten");
        assert_ne!(before, k(spec), "{file}: a rewritten file asks again");
    }

    // A file that appears after the approval asks again too.
    let spec = stdio("node", &["later.js"], &[]);
    let before = k(&spec);
    write("later.js", "new");
    assert_ne!(before, k(&spec));

    // Files outside the root are not the repository's: their content is not keyed (so a
    // tool update does not ask again), only the command line naming them.
    let spec = stdio("node", &[&elsewhere], &[]);
    let before = k(&spec);
    std::fs::write(&elsewhere, "outside v2").unwrap();
    assert_eq!(before, k(&spec));
    // A link inside the root that is re-pointed elsewhere asks again.
    let other_target = outside.path().canonicalize().unwrap().join("other.js");
    std::fs::write(&other_target, "x").unwrap();
    let spec = stdio("node", &["out-link.js"], &[]);
    std::os::unix::fs::symlink(&elsewhere, repo.join("out-link.js")).unwrap();
    let before = k(&spec);
    std::fs::remove_file(repo.join("out-link.js")).unwrap();
    std::os::unix::fs::symlink(&other_target, repo.join("out-link.js")).unwrap();
    assert_ne!(before, k(&spec));

    // A file too big to hash, or one that cannot be read as a file (a FIFO, which must
    // not block), can never match an approval: it asks every time.
    write("big.js", &"x".repeat(1024 * 1024 + 1));
    let spec = stdio("node", &["big.js"], &[]);
    assert_ne!(k(&spec), k(&spec));
    let fifo = std::ffi::CString::new(repo.join("fifo.js").to_str().unwrap()).unwrap();
    // SAFETY: mkfifo with a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let spec = stdio("node", &["fifo.js"], &[]);
    assert_ne!(k(&spec), k(&spec));

    // Words that name nothing (flags, package names) keep the key stable.
    let spec = stdio("npx", &["-y", "@scope/server@1.2.3", "--port=8080"], &[]);
    assert_eq!(k(&spec), k(&spec));
}
