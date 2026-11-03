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
    // Variable values are not part of the key; names are.
    assert_eq!(
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
    assert_eq!(names.len(), 2, "{names:?}"); // the file and the symlink
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
    // Header values are not part of the key.
    let mut same = http("https://a.example/mcp", &["Authorization"]);
    if let ServerSpec::Http { headers, .. } = &mut same {
        headers.insert("Authorization".into(), "other".into());
    }
    assert_eq!(h, key(repo.path(), "s", &same));

    // A corrupt store is an empty one, and approving rewrites it.
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(APPROVALS_FILE);
    std::fs::write(&path, b"{not json").unwrap();
    let mut store = ApprovalStore::open(&path);
    assert!(!store.is_approved(&k));
    store.approve(&k, repo.path(), "s").unwrap();
    assert!(ApprovalStore::open(&path).is_approved(&k));
}
