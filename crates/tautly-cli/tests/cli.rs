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

#[test]
fn without_a_mode_it_says_what_to_use() {
    let s = Scratch::new();
    let out = s.run(&[]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
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
    for command in [
        "prompt", "sessions", "models", "undo", "memory", "index", "mcp",
    ] {
        assert!(help.contains(command), "{command} missing from:\n{help}");
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
        said.contains("1 updated") && said.contains("1 added"),
        "{said}"
    );
    let file = std::fs::read_to_string(s.home.path().join("models.toml")).unwrap();
    assert!(
        file.contains("claude-sonnet-5-5") && file.contains("gateway-1"),
        "{file}"
    );
    assert!(
        !file.contains("no-price") && !file.contains("no-tools") && !file.contains("elsewhere"),
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
fn memory_list_without_entries_says_so() {
    let s = Scratch::new();
    let out = s.run(&["memory", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("no memory"), "{}", stdout(&out));
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
fn mcp_import_defaults_to_claude_json_in_home_else_mcp_json_here() {
    let s = Scratch::new();
    // Neither exists.
    let out = s.run(&["mcp", "import"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains(".claude.json"), "{}", stderr(&out));
    // ./.mcp.json.
    s.write(".mcp.json", r#"{"mcpServers": {"here": {"command": "x"}}}"#);
    let out = s.run(&["mcp", "import"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        std::fs::read_to_string(s.config())
            .unwrap()
            .contains("[mcp.servers.here]")
    );
    // ~/.claude.json wins over it.
    std::fs::write(
        s.home.path().join(".claude.json"),
        r#"{"mcpServers": {"homey": {"command": "y"}}}"#,
    )
    .unwrap();
    let out = s.run(&["mcp", "import"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let config = std::fs::read_to_string(s.config()).unwrap();
    assert!(config.contains("[mcp.servers.homey]"), "{config}");
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
