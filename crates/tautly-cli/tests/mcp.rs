//! `tautly mcp list` against the `echo_server` fixture (crate `tautly-mcp-fixture`).

use std::process::{Command, Output};
use std::sync::OnceLock;

use tempfile::TempDir;

/// The fixture binary next to the `tautly` binary, brought up to date once per test binary
/// (`cargo test -p tautly` alone does not rebuild another package's binary).
fn fixture() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let tautly = std::path::PathBuf::from(env!("CARGO_BIN_EXE_tautly"));
        let bin = tautly.parent().unwrap().join("echo_server");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "tautly-mcp-fixture", "--bin", "echo_server"])
            .status()
            .unwrap();
        assert!(status.success(), "could not build the echo_server fixture");
        assert!(bin.exists(), "no fixture at {}", bin.display());
        bin.to_string_lossy().into_owned()
    })
}

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
fn mcp_list_without_servers_says_so() {
    let s = Scratch::new();
    let out = s.run(&["mcp", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("no MCP servers"), "{}", stdout(&out));
}

#[test]
fn mcp_list_shows_state_source_approval_and_tools() {
    let s = Scratch::new();
    let bin = fixture();
    std::fs::write(
        s.home.path().join("config.toml"),
        format!("[mcp.servers.echo]\ncommand = \"{bin}\"\nargs = [\"stdio\"]\n"),
    )
    .unwrap();
    // A repository server nobody approved, and one that cannot start (user config).
    std::fs::write(
        s.work.path().join(".mcp.json"),
        format!(r#"{{"mcpServers": {{"repo": {{"command": "{bin}", "args": ["stdio"]}}}}}}"#),
    )
    .unwrap();
    let out = s.run(&["mcp", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let list = stdout(&out);
    let echo = list.lines().find(|l| l.starts_with("echo")).expect(&list);
    let cols: Vec<&str> = echo.split_whitespace().collect();
    assert_eq!(&cols[..4], ["echo", "user", "trusted", "ready"], "{echo}");
    assert!(cols[4].parse::<u32>().unwrap() > 0, "tool count: {echo}");
    let repo = list.lines().find(|l| l.starts_with("repo")).expect(&list);
    assert!(
        repo.contains("mcp_json") && repo.contains("pending") && repo.contains("not started"),
        "{repo}"
    );
    assert!(
        list.contains("--approve-mcp") || list.contains("interactive"),
        "how to approve it: {list}"
    );
}

#[test]
fn mcp_list_shows_a_server_that_fails() {
    let s = Scratch::new();
    std::fs::write(
        s.home.path().join("config.toml"),
        "[mcp.servers.broken]\ncommand = \"/nonexistent/server\"\n",
    )
    .unwrap();
    let out = s.run(&["mcp", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let list = stdout(&out);
    let row = list.lines().find(|l| l.starts_with("broken")).expect(&list);
    assert!(row.contains("failed"), "{row}");
}
