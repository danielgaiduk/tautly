//! The approval key of a repository-declared stdio MCP server, which reads the command
//! line (argv words and shell tokens) for the repository files it runs. Input: the
//! command, then its arguments, `\0`-separated; a word `K=V` after an `\x01` is a
//! variable instead. Never panics; the work is bounded however long or deep the words
//! (libFuzzer's `-timeout` and `-rss_limit_mb` catch a blow-up); and a rememberable key
//! is the same on every call.
#![no_main]

use std::collections::BTreeMap;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use tautly_mcp::ServerSpec;
use tautly_mcp::approvals::approval_key;

/// A repository with the files command lines usually name.
static ROOT: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir_all(p.join("pkg/sub/__pycache__")).unwrap();
    std::fs::create_dir_all(p.join("scripts")).unwrap();
    for (f, text) in [
        ("package.json", "{}"),
        ("Makefile", "all:\n"),
        ("scripts/mcp.js", "x"),
        ("pkg/__init__.py", ""),
        ("pkg/sub/__init__.py", ""),
        ("pkg/sub/tool.py", "x"),
        ("pkg/sub/__pycache__/tool.cpython-312.pyc", "x"),
    ] {
        std::fs::write(p.join(f), text).unwrap();
    }
    dir
});

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    let (argv, vars) = input.split_once('\x01').unwrap_or((input, ""));
    let mut words = argv.split('\0').map(str::to_string);
    let command = words.next().unwrap_or_default();
    let env: BTreeMap<String, String> = vars
        .split('\0')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let spec = ServerSpec::Stdio {
        command,
        args: words.collect(),
        env,
        cwd: None,
    };
    let root = ROOT.path();
    let a = approval_key(root, "s", &spec);
    let b = approval_key(root, "s", &spec);
    if a.rememberable {
        assert_eq!(a, b, "a rememberable key changed between calls");
    }
});
