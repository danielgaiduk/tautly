//! The approval key and store. The engine-level approval flow (events, headless,
//! shadowing, denial) is tested in `tautly-core/tests/mcp.rs`.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

/// Review N1 (ruled scope): the launch shapes whose entry code a word does not name as a
/// file exactly still key that code: directories (recursively), `python -m`, attached
/// short flags, `sh -c` strings, and runner manifests.
#[test]
fn launch_shapes_key_the_code_they_run() {
    use tautly_mcp::approvals::approval_key;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    let write = |rel: &str, text: &str| {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    let k = |spec: &ServerSpec| key(&repo, "s", spec);
    let cases: Vec<(&str, ServerSpec)> = vec![
        ("pre.js", stdio("node", &["-r./pre.js", "main.js"], &[])),
        ("pkg/__main__.py", stdio("python3", &["-m", "pkg"], &[])),
        ("tool.py", stdio("python3", &["-m", "tool"], &[])),
        // A sourceless module (review S2-I1), and one in a package.
        ("srcless.pyc", stdio("python3", &["-m", "srcless"], &[])),
        ("pk2/m.pyc", stdio("python3", &["-m", "pk2.m"], &[])),
        ("a/b/c.py", stdio("python3", &["-m", "a.b.c"], &[])),
        ("index.js", stdio("node", &["."], &[])),
        ("dist/index.js", stdio("node", &["dist"], &[])),
        ("dist/deep/lib.js", stdio("node", &["dist"], &[])),
        (
            "srv/main.js",
            stdio("sh", &["-c", "cd srv && node main.js"], &[]),
        ),
        (
            "run.sh",
            stdio("bash", &["-c", "./run.sh --stdio|tee x"], &[]),
        ),
        ("package.json", stdio("npm", &["run", "mcp"], &[])),
        ("package.json", stdio("npx", &["-y", "tsx", "x.ts"], &[])),
        ("pyproject.toml", stdio("uv", &["run", "server"], &[])),
        ("Makefile", stdio("make", &["serve"], &[])),
        ("Cargo.toml", stdio("cargo", &["run"], &[])),
        ("justfile", stdio("just", &["mcp"], &[])),
        ("deno.json", stdio("deno", &["task", "mcp"], &[])),
        ("bin/srv.js", stdio("node", &["--import=./bin/srv.js"], &[])),
        // Review S2-M1: combined option letters ending in `m`, and parent packages.
        ("tool.py", stdio("python3", &["-um", "tool"], &[])),
        ("tool.py", stdio("python3", &["-Pm", "tool"], &[])),
        ("tool.py", stdio("python3", &["-umtool"], &[])),
        ("pq/__init__.py", stdio("python3", &["-m", "pq.sub"], &[])),
        (
            "pq/sub/__init__.py",
            stdio("python3", &["-m", "pq.sub.x"], &[]),
        ),
        // Review S2-M2: a runner anywhere in the command line.
        ("package.json", stdio("sh", &["-c", "npm run mcp"], &[])),
        (
            "package.json",
            stdio("/usr/bin/env", &["npm", "run", "mcp"], &[]),
        ),
        // Review S2-M3: the files a runner prefers or is configured by.
        ("GNUmakefile", stdio("make", &["serve"], &[])),
        ("makefile", stdio("make", &["serve"], &[])),
        ("Justfile", stdio("just", &["mcp"], &[])),
        (".justfile", stdio("just", &["mcp"], &[])),
        (".npmrc", stdio("npm", &["run", "mcp"], &[])),
        (".yarnrc.yml", stdio("yarn", &["mcp"], &[])),
        ("bunfig.toml", stdio("bun", &["run", "mcp"], &[])),
        (".cargo/config.toml", stdio("cargo", &["run"], &[])),
        ("deno.jsonc", stdio("deno", &["task", "mcp"], &[])),
        // Review S3-M1: `-m` inside one shell word takes the next token of that word.
        ("tool.py", stdio("sh", &["-c", "python3 -m tool"], &[])),
        ("tool.py", stdio("sh", &["-c", "python3 -um tool"], &[])),
        (
            "pkg/__main__.py",
            stdio("sh", &["-c", "exec python3 -O -m pkg"], &[]),
        ),
        // Review S3-M3: a variable's value names files like an argv word does.
        (
            "x.js",
            stdio(
                "node",
                &["server.js"],
                &[("NODE_OPTIONS", "--require ./x.js")],
            ),
        ),
        (
            "sitecustomize.py",
            stdio("python3", &["-m", "pkg"], &[("PYTHONPATH", ".")]),
        ),
        (
            "vendor/py/lib/site.py",
            stdio("python3", &["-m", "pkg"], &[("PYTHONHOME", "vendor/py")]),
        ),
    ];
    // Review S3-I1: argv that can switch the private bytecode cache off (`-E`, `-Em`,
    // `-sE`, `exec python3 -E` in a shell word) is never remembered, so no key of it is
    // stable: see `argv_that_can_drop_the_bytecode_prefix_is_never_remembered`.
    for (file, spec) in &cases {
        write(file, "v1");
        let before = k(spec);
        assert_eq!(before, k(spec), "{file}: stable");
        write(file, "v2: rewritten");
        assert_ne!(
            before,
            k(spec),
            "{file} in {spec:?}: a rewritten file asks again"
        );
    }

    // A directory's skipped parts (and tautly's own scratch output) do not count; nor
    // does the timestamp-checked bytecode Python itself writes to a `__pycache__`.
    let spec = stdio("node", &["."], &[]);
    let before = k(&spec);
    for skipped in [
        ".git/HEAD",
        "node_modules/x/index.js",
        "target/debug/x",
        ".venv/lib/x.py",
        ".tautly/tmp/spill/0123.txt",
    ] {
        write(skipped, "changes every run");
        assert_eq!(before, k(&spec), "{skipped}");
    }
    write_pyc(
        &repo,
        "pkg/__pycache__/x.cpython-39.pyc",
        0,
        b"timestamp v1",
    );
    assert_eq!(before, k(&spec), "a timestamp pyc");
    write_pyc(
        &repo,
        "pkg/__pycache__/x.cpython-39.pyc",
        0,
        b"timestamp v2",
    );
    assert_eq!(before, k(&spec), "a rewritten timestamp pyc");
    write(
        "pkg/__pycache__/notes.txt",
        "not bytecode, which Python never reads here",
    );
    assert_eq!(before, k(&spec), "a non-pyc file in a cache");
    // A directory over the bounds can never be remembered: it asks at every start.
    write("dist/huge.bin", &"x".repeat(1024 * 1024 + 1));
    let spec = stdio("node", &["dist"], &[]);
    assert!(!approval_key(&repo, "s", &spec).rememberable);
    assert_ne!(k(&spec), k(&spec));
    // A link inside a named directory is keyed by its target, not followed.
    std::fs::remove_file(repo.join("dist/huge.bin")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", repo.join("dist/link")).unwrap();
    let before = k(&spec);
    std::fs::remove_file(repo.join("dist/link")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", repo.join("dist/link")).unwrap();
    assert_ne!(before, k(&spec));
    assert!(approval_key(&repo, "s", &spec).rememberable);

    // Review S2-M5: names and link targets may hold newlines; two different trees must
    // not encode alike (a re-pointed link plus a renamed file).
    std::fs::remove_file(repo.join("dist/link")).unwrap();
    std::os::unix::fs::symlink("evil.js\nm", repo.join("dist/lib.js")).unwrap();
    write("dist/m", "same");
    let a = k(&spec);
    std::fs::remove_file(repo.join("dist/lib.js")).unwrap();
    std::fs::remove_file(repo.join("dist/m")).unwrap();
    std::os::unix::fs::symlink("evil.js", repo.join("dist/lib.js")).unwrap();
    write("dist/m\nm", "same");
    assert_ne!(a, k(&spec));
}

/// A `.pyc` at `rel` below `repo` with the given flags word (0: timestamp-checked, as
/// Python writes them; non-zero: hash-based, which only a tool writes) and some body.
fn write_pyc(repo: &Path, rel: &str, flags: u32, body: &[u8]) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut bytes = vec![0x61, 0x0d, 0x0d, 0x0a]; // a CPython magic number
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]); // mtime and size, or the source hash
    bytes.extend_from_slice(body);
    std::fs::write(p, bytes).unwrap();
}

/// Review S3-I1 (a): hash-based bytecode in a `__pycache__` is keyed, for the module a
/// `-m` names, for its parent packages and in every directory walked; the timestamp
/// bytecode Python writes is not, so its own cache does not ask again.
#[test]
fn hash_based_bytecode_is_keyed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    let write = |rel: &str, text: &str| {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    };
    let k = |spec: &ServerSpec| key(&repo, "s", spec);
    write("pkg/__init__.py", "");
    write("pkg/__main__.py", "print('BENIGN')");
    write("tool.py", "print('BENIGN')");
    write("pq/__init__.py", "");
    write("pq/sub.py", "print('BENIGN')");
    write("app/index.js", "// v1");
    let cases: Vec<(&str, ServerSpec)> = vec![
        // The module run, in a package and at the top level.
        (
            "pkg/__pycache__/__main__.cpython-313.pyc",
            stdio("python3", &["-m", "pkg"], &[]),
        ),
        (
            "__pycache__/tool.cpython-313.pyc",
            stdio("python3", &["-m", "tool"], &[]),
        ),
        // The parent package's `__init__`, which runs first.
        (
            "pq/__pycache__/__init__.cpython-313.pyc",
            stdio("python3", &["-m", "pq.sub"], &[]),
        ),
        // An optimized variant, and a `-m` inside a shell word.
        (
            "pkg/__pycache__/__main__.cpython-313.opt-1.pyc",
            stdio("sh", &["-c", "exec python3 -O -m pkg"], &[]),
        ),
        // Below a named directory.
        (
            "app/lib/__pycache__/x.cpython-313.pyc",
            stdio("node", &["app"], &[]),
        ),
    ];
    for (pyc, spec) in &cases {
        let before = k(spec);
        assert_eq!(before, k(spec), "{pyc}: stable");
        // Python's own cache: not keyed, however it changes.
        write_pyc(&repo, pyc, 0, b"timestamp v1");
        assert_eq!(before, k(spec), "{pyc}: a timestamp pyc");
        write_pyc(&repo, pyc, 0, b"timestamp v2");
        assert_eq!(before, k(spec), "{pyc}: a rewritten timestamp pyc");
        // Hash-based (checked, 3, or unchecked, 1): keyed, like the source.
        write_pyc(&repo, pyc, 1, b"unchecked v1");
        let unchecked = k(spec);
        assert_ne!(before, unchecked, "{pyc}: an unchecked-hash pyc asks again");
        assert_eq!(unchecked, k(spec), "{pyc}: stable with the pyc");
        write_pyc(&repo, pyc, 1, b"unchecked v2");
        assert_ne!(unchecked, k(spec), "{pyc}: a rewritten unchecked-hash pyc");
        write_pyc(&repo, pyc, 3, b"checked v1");
        assert_ne!(before, k(spec), "{pyc}: a checked-hash pyc asks again");
        std::fs::remove_file(repo.join(pyc)).unwrap();
        assert_eq!(before, k(spec), "{pyc}: removed");
    }
    // Hash-based bytecode reached through a link counts (Python reads it); a dangling
    // link, which an import passes over, adds nothing.
    let spec = stdio("python3", &["-m", "pkg"], &[]);
    let before = k(&spec);
    let link = repo.join("pkg/__pycache__/__main__.cpython-313.pyc");
    std::os::unix::fs::symlink("../../elsewhere.bin", &link).unwrap();
    assert_eq!(before, k(&spec), "a dangling link");
    write_pyc(&repo, "elsewhere.bin", 1, b"unchecked, via a link");
    assert_ne!(before, k(&spec), "an unchecked-hash pyc through a link");
    std::fs::remove_file(&link).unwrap();
    assert_eq!(before, k(&spec));
    // Review S4-M2: a `__pycache__` that is a link to a directory below a walked tree
    // is walked as a cache too (Python follows it), wherever it points.
    let spec = stdio("node", &["app"], &[]);
    std::fs::create_dir_all(repo.join("node_modules/c")).unwrap();
    std::fs::create_dir_all(repo.join("app/vendor")).unwrap();
    std::os::unix::fs::symlink("../../node_modules/c", repo.join("app/vendor/__pycache__"))
        .unwrap();
    let before = k(&spec);
    assert_eq!(before, k(&spec));
    write_pyc(
        &repo,
        "node_modules/c/helper.cpython-313.pyc",
        0,
        b"timestamp",
    );
    assert_eq!(before, k(&spec), "a timestamp pyc behind a linked cache");
    write_pyc(
        &repo,
        "node_modules/c/helper.cpython-313.pyc",
        1,
        b"unchecked",
    );
    assert_ne!(
        before,
        k(&spec),
        "an unchecked-hash pyc behind a linked cache"
    );
    std::fs::remove_file(repo.join("node_modules/c/helper.cpython-313.pyc")).unwrap();
    assert_eq!(before, k(&spec));
    // Every case above may be remembered: no bound was hit.
    for (_, spec) in &cases {
        assert!(tautly_mcp::approvals::approval_key(&repo, "s", spec).rememberable);
    }
}

/// Review S3-I1 (b): a command line that can drop the forced `PYTHONPYCACHEPREFIX` in
/// the server (Python's `-E`/`-I`, `env -i`/`-u`, `unset`, or naming the variable) is
/// never remembered: the key matches nothing stored, and asks at every start.
#[test]
fn argv_that_can_drop_the_bytecode_prefix_is_never_remembered() {
    use tautly_mcp::approvals::approval_key;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().canonicalize().unwrap();
    let never: Vec<ServerSpec> = vec![
        stdio("python3", &["-E", "-m", "pkg"], &[]),
        stdio("python3", &["-Em", "pkg"], &[]),
        stdio("python3", &["-sE", "-m", "pkg"], &[]),
        stdio("python3", &["-I", "-m", "pkg"], &[]),
        stdio("python3", &["-Im", "pkg"], &[]),
        stdio("python", &["-E", "server.py"], &[]),
        stdio("sh", &["-c", "exec python3 -E -m pkg"], &[]),
        stdio(
            "/usr/bin/env",
            &["-u", "PYTHONPYCACHEPREFIX", "python3", "-m", "pkg"],
            &[],
        ),
        stdio("env", &["-i", "python3", "-m", "pkg"], &[]),
        stdio("env", &["-iS", "python3 -m pkg"], &[]),
        stdio(
            "env",
            &["--ignore-environment", "python3", "-m", "pkg"],
            &[],
        ),
        stdio(
            "env",
            &["--unset=PYTHONPYCACHEPREFIX", "python3", "-m", "pkg"],
            &[],
        ),
        stdio(
            "sh",
            &["-c", "env -u PYTHONPYCACHEPREFIX python3 -m pkg"],
            &[],
        ),
        stdio(
            "sh",
            &["-c", "unset PYTHONPYCACHEPREFIX; exec python3 -m pkg"],
            &[],
        ),
        stdio(
            "sh",
            &["-c", "PYTHONPYCACHEPREFIX= exec python3 -m pkg"],
            &[],
        ),
        stdio(
            "env",
            &["PYTHONPYCACHEPREFIX=/tmp/x", "python3", "-m", "pkg"],
            &[],
        ),
        // Review S4-I1: `-X pycache_prefix` overrides the variable.
        stdio("python3", &["-X", "pycache_prefix=/", "-m", "tool"], &[]),
        stdio("python3", &["-Xpycache_prefix=/"], &[]),
        stdio(
            "sh",
            &["-c", "exec python3 -X pycache_prefix=/ -m tool"],
            &[],
        ),
        stdio("python3", &["-X", "pycache_prefix=/", "-m", "a.b"], &[]),
        // Review S4-M1: other spellings of `env -i`, env's options with a value, a
        // flag held in a variable, and shell quotes.
        stdio("env", &["-", "python3", "-m", "pkg"], &[]),
        stdio("env", &["--ignore-env", "python3", "-m", "pkg"], &[]),
        stdio("env", &["--i", "python3", "-m", "pkg"], &[]),
        stdio(
            "env",
            &["-P", "/usr/bin", "-i", "python3", "-m", "pkg"],
            &[],
        ),
        stdio("env", &["-C", "/tmp", "-i", "python3", "-m", "pkg"], &[]),
        stdio(
            "env",
            &["--chdir", "/tmp", "-", "python3", "-m", "pkg"],
            &[],
        ),
        stdio("sh", &["-c", "exec python3 $F -m tool"], &[("F", "-E")]),
        stdio("sh", &["-c", "exec python3 -\"E\" -m tool"], &[]),
        stdio("sh", &["-c", "exec python3 -'E' -m tool"], &[]),
    ];
    for spec in &never {
        let k = approval_key(&repo, "s", spec);
        assert!(!k.rememberable, "{spec:?}");
        assert_ne!(k.key, approval_key(&repo, "s", spec).key, "{spec:?}");
    }
    // Ordinary command lines, including Python's other flags, `-i` and `-u` where no
    // `env` runs, and variables of other names, are remembered.
    let remembered: Vec<ServerSpec> = vec![
        stdio("python3", &["-m", "pkg"], &[]),
        stdio("python3", &["-u", "-m", "pkg"], &[]),
        stdio("python3", &["-um", "pkg"], &[]),
        stdio("python3", &["-i", "server.py"], &[]),
        stdio("python3", &["-X", "dev", "-m", "pkg"], &[]),
        stdio("python3", &["--version"], &[]),
        stdio("env", &["FOO=bar", "python3", "-m", "pkg"], &[]),
        stdio(
            "env",
            &["PYTHONUNBUFFERED=1", "python3", "-u", "-m", "pkg"],
            &[],
        ),
        stdio("env", &["-S", "python3 -u -m pkg"], &[]),
        stdio("sh", &["-c", "exec python3 -m pkg"], &[]),
        stdio(
            "sh",
            &["-c", "exec python3 -c 'import srv; srv.main()'"],
            &[],
        ),
        stdio("sh", &["-c", "exec python3 -u -i server.py"], &[]),
        stdio("node", &["-e", "1"], &[]),
        stdio("node", &["--env-file=.env", "server.js"], &[]),
        stdio(
            "env",
            &["-P", "/usr/bin", "python3", "-u", "-m", "pkg"],
            &[],
        ),
        stdio("python3", &["-X", "dev", "-m", "pkg"], &[("F", "-u")]),
    ];
    for spec in &remembered {
        let k = approval_key(&repo, "s", spec);
        assert!(k.rememberable, "{spec:?}");
        assert_eq!(k.key, approval_key(&repo, "s", spec).key, "{spec:?}");
    }
}
