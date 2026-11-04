//! The `tautly` binary's argument handling. These runs never reach a provider: they stop
//! at usage errors, before any session starts. Each runs in a scratch directory with its
//! own `TAUTLY_HOME`.

use std::process::{Command, Output};

use tempfile::TempDir;

struct Scratch {
    home: TempDir,
    work: TempDir,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tautly"))
            .args(args)
            .current_dir(self.work.path())
            .env("TAUTLY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_lists_the_flags() {
    let s = Scratch::new();
    let out = s.run(&["--help"]);
    assert!(out.status.success());
    let help = stdout(&out);
    for flag in [
        "-p, --prompt",
        "--output",
        "--approve <MODE>",
        "--model",
        "--budget-usd",
        "--approve-memory",
        "--approve-mcp",
        "--continue",
        "--resume",
        "--rpc",
        "--no-checkpoints",
        "--no-footer",
        "auto-edit",
        "yolo",
    ] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
    assert!(!help.contains("mock-script"), "the test flag is hidden");
    let out = s.run(&["--version"]);
    assert!(out.status.success());
    assert!(stdout(&out).starts_with("tautly "), "{}", stdout(&out));
}

#[test]
fn bad_flags_are_usage_errors() {
    let s = Scratch::new();
    for args in [
        &["--approve", "never", "--rpc"][..],
        &["-p", "hi", "--rpc"],
        &["-p", "hi", "--continue", "--resume", "01ABC"],
        &["-p", "hi", "--budget-usd", "-1"],
        &["-p", "hi", "--budget-usd", "lots"],
        &["--rpc", "--output", "json"],
        &["-p", "hi", "--output", "yaml"],
    ] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
    }
}

/// Without `-p`, `--rpc` or a command the REPL runs, which needs a terminal (these runs
/// have none).
#[test]
fn without_a_mode_it_says_what_to_use() {
    let s = Scratch::new();
    let out = s.run(&[]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("the REPL needs a terminal"), "{err}");
    assert!(err.contains("-p") && err.contains("--rpc"), "{err}");
}

#[test]
fn an_unknown_provider_is_refused_before_anything_starts() {
    let s = Scratch::new();
    let out = s.run(&["-p", "hi", "--model", "acme/turbo"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("acme/turbo") && err.contains("anthropic"),
        "{err}"
    );
    assert!(stdout(&out).is_empty());
}

#[test]
fn continue_without_an_earlier_session_is_an_error() {
    let s = Scratch::new();
    let out = s.run(&["-p", "hi", "--continue", "--model", "openai/gpt-5"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("no earlier session"),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------- utility commands

impl Scratch {
    fn write(&self, name: &str, text: &str) {
        let path = self.work.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The user config file (`TAUTLY_HOME` is the config directory itself).
    fn config(&self) -> std::path::PathBuf {
        self.home.path().join("config.toml")
    }
}

#[test]
fn help_lists_the_commands() {
    let s = Scratch::new();
    let help = stdout(&s.run(&["--help"]));
    // The subcommand lines ("  sessions  List, show ..."), not words in the prose.
    let commands = help
        .split("Commands:")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .unwrap_or_default();
    for command in [
        "prompt", "sessions", "models", "undo", "memory", "index", "mcp", "stats", "receipt",
    ] {
        assert!(
            commands
                .lines()
                .any(|l| l.starts_with(&format!("  {command} "))),
            "{command} missing from:\n{help}"
        );
    }
}

#[test]
fn a_command_does_not_go_with_a_prompt() {
    let s = Scratch::new();
    for args in [
        &["-p", "hi", "models", "list"][..],
        &["--rpc", "index", "status"],
        &["--continue", "undo"],
    ] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn prompt_show_prints_system_prompt() {
    let s = Scratch::new();
    let out = s.run(&["prompt", "show"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        tautly_core::system_prompt().trim_end()
    );
    assert!(!stdout(&out).trim().is_empty());
}

#[test]
fn prompt_show_full_without_a_session_says_so() {
    let s = Scratch::new();
    let out = s.run(&["prompt", "show", "--full"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no request"), "{}", stderr(&out));
}

#[test]
fn sessions_commands_on_an_empty_project() {
    let s = Scratch::new();
    let out = s.run(&["sessions", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("no sessions"), "{}", stdout(&out));
    for args in [
        &["sessions", "show", "01ABC"][..],
        &["sessions", "export", "01ABC"],
        &["sessions", "delete", "01ABC"],
        &["undo"],
        &["undo", "--session", "01ABC"],
    ] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&out).contains("no session") || stderr(&out).contains("nothing to undo"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn models_list_shows_the_catalog() {
    let s = Scratch::new();
    let out = s.run(&["models", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let list = stdout(&out);
    assert!(list.contains("anthropic/claude-sonnet-5-5"), "{list}");
    assert!(list.contains("200000"), "{list}");
}

/// A models.dev-format document: one model the catalog knows with new numbers, one it
/// does not know, one without a price, one without tool calls, and a provider Tautly has
/// no adapter for.
fn models_dev() -> serde_json::Value {
    serde_json::json!({
        "anthropic": {"id": "anthropic", "models": {
            "claude-sonnet-5-5": {
                "id": "claude-sonnet-5-5", "tool_call": true,
                "modalities": {"input": ["text", "image"], "output": ["text"]},
                "cost": {"input": 2.5, "output": 12.0, "cache_read": 0.25, "cache_write": 3.0},
                "limit": {"context": 123456, "output": 7777}
            },
            "claude-new-9": {
                "id": "claude-new-9", "tool_call": true,
                "cost": {"input": 4.0, "output": 20.0},
                "limit": {"context": 100000, "output": 9000}
            }
        }},
        "openai": {"id": "openai", "models": {
            "gateway-1": {
                "id": "gateway-1", "tool_call": true,
                "modalities": {"input": ["text"], "output": ["text"]},
                "cost": {"input": 0.4, "output": 1.6},
                "limit": {"context": 64000, "output": 8000}
            },
            "no-price": {"id": "no-price", "tool_call": true,
                         "limit": {"context": 64000, "output": 8000}},
            "no-tools": {"id": "no-tools", "tool_call": false,
                         "cost": {"input": 1.0, "output": 2.0},
                         "limit": {"context": 64000, "output": 8000}}
        }},
        "elsewhere": {"id": "elsewhere", "models": {
            "m": {"id": "m", "tool_call": true, "cost": {"input": 1.0, "output": 2.0},
                  "limit": {"context": 1000, "output": 100}}
        }}
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_update_from_local_fixture_server() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models_dev()))
        .mount(&server)
        .await;
    let url = format!("{}/api.json", server.uri());
    let out = tokio::task::spawn_blocking(move || {
        let s = Scratch::new();
        let out = s.run(&["models", "update", "--from", &url]);
        (s, out)
    })
    .await
    .unwrap();
    let (s, out) = out;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(
        said.contains("1 updated") && said.contains("2 added"),
        "{said}"
    );
    let file = std::fs::read_to_string(s.home.path().join("models.toml")).unwrap();
    // A built-in model keeps only what update changes: the rest stays the running
    // version's. Models nobody knew get whole entries.
    let sonnet_table: String = file
        .split("\n[models.")
        .find(|t| t.starts_with("\"anthropic/claude-sonnet-5-5\"]"))
        .unwrap()
        .to_string();
    for frozen in [
        "reasoning",
        "edit_format",
        "cache =",
        "parallel_tools",
        "tier",
    ] {
        assert!(
            !sonnet_table.contains(frozen),
            "{frozen} frozen in:\n{file}"
        );
    }
    let gateway_table = file
        .split("\n[models.")
        .find(|t| t.starts_with("\"openai/gateway-1\"]"))
        .unwrap();
    assert!(gateway_table.contains("edit_format"), "{file}");
    // Missing cache prices: 1.25x input for Anthropic, the input price for the others.
    let table = |name: &str| {
        file.split("\n[models.")
            .find(|t| t.starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing from:\n{file}"))
            .to_string()
    };
    assert!(
        table("\"anthropic/claude-new-9\".price]").contains("cache_write = 5.0"),
        "{file}"
    );
    assert!(
        table("\"openai/gateway-1\".price]").contains("cache_write = 0.4"),
        "{file}"
    );

    // The next run of anything reads it: the catalog now has the new numbers.
    let list = stdout(&s.run(&["models", "list"]));
    let sonnet = list
        .lines()
        .find(|l| l.contains("anthropic/claude-sonnet-5-5"))
        .unwrap();
    assert!(
        sonnet.contains("123456") && sonnet.contains("2.5"),
        "{sonnet}"
    );
    let gateway = list
        .lines()
        .find(|l| l.contains("openai/gateway-1"))
        .unwrap();
    assert!(
        gateway.contains("64000") && gateway.contains("0.4"),
        "{gateway}"
    );

    // Updating again changes nothing.
    let again = s.run(&[
        "models",
        "update",
        "--from",
        &format!("{}/api.json", server.uri()),
    ]);
    assert!(stdout(&again).contains("0 updated"), "{}", stdout(&again));
}

#[test]
fn models_update_reports_a_failing_source_and_keeps_the_file() {
    let s = Scratch::new();
    std::fs::write(s.home.path().join("models.toml"), "# mine\n").unwrap();
    let out = s.run(&["models", "update", "--from", "http://127.0.0.1:1/api.json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("127.0.0.1"), "{}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(s.home.path().join("models.toml")).unwrap(),
        "# mine\n"
    );
}

const MEMORY_STALE: &str = "---\nid: alpha-note\ntitle: \"Alpha note\"\nkind: fact\nanchors:\n  - path: src.rs\n    symbol: \"alpha\"\n    hash: \"deadbeef\"\ncreated: 2026-01-01T00:00:00\n---\nAlpha does a thing.\n";
const MEMORY_FRESH: &str = "---\nid: plain-note\ntitle: \"Plain note\"\nkind: convention\nanchors: []\ncreated: 2026-01-02T00:00:00\n---\nUse tabs.\n";

#[test]
fn memory_list_shows_stale_marker() {
    let s = Scratch::new();
    s.write("src.rs", "fn alpha() {}\n");
    s.write(".tautly/memory/alpha-note.md", MEMORY_STALE);
    s.write(".tautly/memory/plain-note.md", MEMORY_FRESH);
    let out = s.run(&["memory", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let list = stdout(&out);
    let alpha = list.lines().find(|l| l.contains("alpha-note")).unwrap();
    let plain = list.lines().find(|l| l.contains("plain-note")).unwrap();
    assert!(
        alpha.contains("[stale]") && alpha.contains("Alpha note"),
        "{list}"
    );
    assert!(
        !plain.contains("[stale]") && plain.contains("convention"),
        "{list}"
    );

    let out = s.run(&["memory", "show", "alpha-note"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(
        shown.contains("Alpha does a thing.")
            && shown.contains("src.rs")
            && shown.contains("stale"),
        "{shown}"
    );
    let out = s.run(&["memory", "show", "missing"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("missing"), "{}", stderr(&out));
}

#[test]
fn memory_show_and_list_escape_control_characters_but_keep_lines() {
    let s = Scratch::new();
    // A repository's memory entry whose body tries to erase and overwrite lines.
    s.write(
        ".tautly/memory/evil.md",
        "---\nid: evil\ntitle: \"Evil\"\nkind: fact\nanchors: []\ncreated: 2026-01-01T00:00:00\n---\n\
         first line\u{1b}[2K\u{1b}[1Aforged\rover\u{7}write\nsecond\tline with C:\\dir\nthird\n",
    );
    for args in [&["memory", "show", "evil"][..], &["memory", "list"]] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_no_control_bytes("stdout", &out.stdout);
        assert_no_control_bytes("stderr", &out.stderr);
    }
    let out = s.run(&["memory", "show", "evil"]);
    let shown = stdout(&out);
    // Escaped, so visible; the entry's lines and tabs and backslashes are as written.
    assert!(
        shown.contains(r"first line\u{1b}[2K\u{1b}[1Aforged\rover\u{7}write"),
        "{shown}"
    );
    assert!(
        shown.contains("\nsecond\tline with C:\\dir\nthird\n"),
        "{shown}"
    );
}

#[test]
fn memory_list_without_entries_says_so() {
    let s = Scratch::new();
    let out = s.run(&["memory", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("no memory"), "{}", stdout(&out));
}

#[test]
fn index_rebuild_refuses_while_a_session_holds_the_index() {
    let s = Scratch::new();
    s.write("a.rs", "fn one() {}\n");
    assert_eq!(s.run(&["index", "status"]).status.code(), Some(0));
    let dir = s.home.path().join("index");
    let lock_file = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".db.lock"))
        .expect("a session leaves the lock file");
    let db = lock_file.with_extension("");
    assert!(db.exists());
    // What a running session does: hold the lock shared.
    let held = std::fs::File::open(&lock_file).unwrap();
    held.lock_shared().unwrap();
    let out = s.run(&["index", "rebuild"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("running tautly") && stderr(&out).contains("stop it"),
        "{}",
        stderr(&out)
    );
    assert!(db.exists(), "the database is untouched");
    // Status (a reader) is fine meanwhile; once the session is gone, rebuild works.
    assert_eq!(s.run(&["index", "status"]).status.code(), Some(0));
    drop(held);
    let out = s.run(&["index", "rebuild"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("files: 1"), "{}", stdout(&out));
}

#[test]
fn index_status_reports_file_and_symbol_counts() {
    let s = Scratch::new();
    s.write("a.rs", "fn one() {}\nfn two() {}\n");
    s.write("b.py", "def three():\n    pass\n");
    let out = s.run(&["index", "status"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(said.contains("files: 2"), "{said}");
    assert!(said.contains("symbols: 3"), "{said}");

    // A rebuild starts from nothing and arrives at the same counts.
    let out = s.run(&["index", "rebuild"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("files: 2") && stdout(&out).contains("symbols: 3"));
    s.write("c.rs", "fn four() {}\n");
    let said = stdout(&s.run(&["index", "status"]));
    assert!(
        said.contains("files: 3") && said.contains("symbols: 4"),
        "{said}"
    );
}

const CLAUDE_JSON: &str = r#"{
  "numStartups": 4,
  "mcpServers": {
    "fs": {"command": "npx", "args": ["-y", "server-fs", "/tmp"], "env": {"A": "1"}},
    "web": {"type": "http", "url": "https://example.com/mcp", "headers": {"X-Key": "k"}},
    "old": {"type": "sse", "url": "https://example.com/sse"}
  }
}"#;

#[test]
fn mcp_import_from_claude_json_preserves_comments() {
    let s = Scratch::new();
    std::fs::write(
        s.config(),
        "# my settings\nmodel = \"openai/gpt-5\" # the usual\n\n[mcp.servers.fs]\ncommand = \"mine\"\n",
    )
    .unwrap();
    s.write("claude.json", CLAUDE_JSON);
    let out = s.run(&["mcp", "import", "--from", "claude.json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(
        said.contains("imported 1") && said.contains("web"),
        "{said}"
    );
    assert!(said.contains("fs") && said.contains("already"), "{said}");
    assert!(said.contains("old") && said.contains("skipped"), "{said}");
    let config = std::fs::read_to_string(s.config()).unwrap();
    assert!(
        config.starts_with("# my settings\nmodel = \"openai/gpt-5\" # the usual\n"),
        "{config}"
    );
    assert!(
        config.contains("command = \"mine\""),
        "an existing server is left alone: {config}"
    );
    assert!(config.contains("[mcp.servers.web]") && config.contains("https://example.com/mcp"));
    assert!(!config.contains("server-fs"), "{config}");
}

#[test]
fn mcp_import_defaults_to_claude_json_and_will_not_trust_the_repository_unasked() {
    use std::os::unix::fs::PermissionsExt as _;

    let s = Scratch::new();
    // Neither exists.
    let out = s.run(&["mcp", "import"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains(".claude.json"), "{}", stderr(&out));

    // The repository's .mcp.json is content a clone brought: without a terminal and without
    // --from it is refused, after showing what it would make trusted.
    s.write(
        ".mcp.json",
        r#"{"mcpServers": {"here": {"command": "x", "args": ["--serve"], "env": {"TOKEN": "s3cret"}}}}"#,
    );
    let out = s.run(&["mcp", "import"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains("here: stdio: x --serve") && err.contains("TOKEN"),
        "what it would import: {err}"
    );
    assert!(!err.contains("s3cret"), "values are not shown: {err}");
    assert!(err.contains("--from"), "{err}");
    assert!(!s.config().exists(), "nothing was written");
    // Found in the work tree's root, not in the directory the command runs in.
    // (The work tree's root is the repository's top: mark this directory as one.)
    std::fs::create_dir(s.work.path().join(".git")).unwrap();
    std::fs::create_dir(s.work.path().join("sub")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tautly"))
        .args(["mcp", "import"])
        .current_dir(s.work.path().join("sub"))
        .env("TAUTLY_HOME", s.home.path())
        .env("HOME", s.home.path())
        .output()
        .unwrap();
    let root = s.work.path().canonicalize().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains(&root.join(".mcp.json").display().to_string()),
        "{}",
        stderr(&out)
    );

    // Naming the file is the user's choice: imported, with what runs shown.
    let out = s.run(&["mcp", "import", "--from", ".mcp.json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("x --serve"), "{}", stdout(&out));
    assert!(
        std::fs::read_to_string(s.config())
            .unwrap()
            .contains("[mcp.servers.here]")
    );

    // ~/.claude.json is the user's own file: the default, imported without asking. A new
    // config file is owner-only (it may hold tokens).
    std::fs::remove_file(s.config()).unwrap();
    std::fs::write(
        s.home.path().join(".claude.json"),
        r#"{"mcpServers": {"homey": {"command": "y"}}}"#,
    )
    .unwrap();
    let out = s.run(&["mcp", "import"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("homey: stdio: y"), "{}", stdout(&out));
    let config = std::fs::read_to_string(s.config()).unwrap();
    assert!(config.contains("[mcp.servers.homey]"), "{config}");
    let mode = std::fs::metadata(s.config()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
}

/// No byte of `bytes` can move the cursor, recolor the screen or end a line early: only
/// printable characters, newlines and tabs.
fn assert_no_control_bytes(what: &str, bytes: &[u8]) {
    for (i, b) in bytes.iter().enumerate() {
        assert!(
            *b >= 0x20 || *b == b'\n' || *b == b'\t',
            "{what} has control byte {b:#04x} at {i}: {:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn mcp_import_escapes_control_characters_of_the_imported_file() {
    let s = Scratch::new();
    // Names from a file (repository content) that try to forge lines and redraw the
    // terminal: new, already in the config, and an entry that is skipped.
    let evil = "evil\u{1b}[2K\nforged: line\r";
    let kept = "kept\u{1b}[31m\nforged: kept";
    let sse = "sse\u{1b}]0;title\u{7}\nforged: skipped";
    let doc = serde_json::json!({"mcpServers": {
        evil: {"command": "x", "args": ["a\u{1b}[1m"]},
        kept: {"command": "y"},
        sse: {"type": "sse", "url": "https://example.com/sse"},
    }});
    std::fs::write(
        s.config(),
        format!("[mcp.servers.{}]\ncommand = \"mine\"\n", {
            // A TOML key with the escape and a newline, as the library writes it.
            let key = kept.replace('\u{1b}', "\\u001b").replace('\n', "\\n");
            format!("\"{key}\"")
        }),
    )
    .unwrap();
    s.write("claude.json", &doc.to_string());

    for out in [
        s.run(&["mcp", "import", "--from", "claude.json"]),
        // The refusal of an unasked, in-tree default prints the same names on stderr.
        {
            s.write(".mcp.json", &doc.to_string());
            s.run(&["mcp", "import"])
        },
    ] {
        assert_no_control_bytes("stdout", &out.stdout);
        assert_no_control_bytes("stderr", &out.stderr);
        let said = format!("{}{}", stdout(&out), stderr(&out));
        // The text is there, escaped, and nothing starts a line of its own.
        assert!(said.contains(r"\u{1b}"), "{said}");
        assert!(
            said.lines().all(|l| !l.trim_start().starts_with("forged")),
            "{said}"
        );
    }
}

#[test]
fn mcp_import_refuses_what_is_not_mcp_json() {
    let s = Scratch::new();
    s.write("bad.json", "this is not json");
    let out = s.run(&["mcp", "import", "--from", "bad.json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("bad.json"), "{}", stderr(&out));
    assert!(!s.config().exists());
}
