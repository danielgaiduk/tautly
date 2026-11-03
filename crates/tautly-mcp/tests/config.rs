//! `[mcp]` parsing, Claude-style JSON and the import into a user `config.toml` (the
//! library side of `tautly mcp import`).

use std::collections::BTreeMap;

use tautly_mcp::config::{import_into_toml, parse_mcp_json};
use tautly_mcp::{McpConfig, ServerSpec};

#[test]
fn mcp_config_defaults_and_server_shapes() {
    let cfg: McpConfig = toml::from_str("").unwrap();
    assert!(cfg.enabled && cfg.import_mcp_json);
    assert_eq!(
        (cfg.timeout_s, cfg.startup_timeout_ms, cfg.max_tools),
        (120, 5000, 50)
    );
    let cfg: McpConfig = toml::from_str(
        r#"
        [servers.files]
        command = "npx"
        args = ["-y", "fs"]
        env = { ROOT = "/tmp" }
        [servers.remote]
        url = "https://mcp.example.com/mcp"
        headers = { Authorization = "Bearer ${TOKEN}" }
        "#,
    )
    .unwrap();
    assert_eq!(
        cfg.servers["files"],
        ServerSpec::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "fs".into()],
            env: BTreeMap::from([("ROOT".into(), "/tmp".into())]),
            cwd: None,
        }
    );
    assert!(
        matches!(&cfg.servers["remote"], ServerSpec::Http { url, .. } if url.host_str() == Some("mcp.example.com"))
    );
    assert_eq!(
        cfg.servers["files"].launch(),
        "stdio: npx -y fs (env: ROOT)"
    );
    assert_eq!(
        cfg.servers["remote"].launch(),
        "http: https://mcp.example.com/mcp (headers: Authorization)"
    );

    for bad in [
        "[servers.x]\nargs = [\"a\"]",
        "[servers.x]\ncommand = \"a\"\nurl = \"https://a.example\"",
        "[servers.x]\ncommand = \"\"",
        "[servers.x]\nurl = \"file:///etc/passwd\"",
        "[servers.x]\ncommand = \"a\"\nheaders = {}",
        "[servers.x]\ncommand = \"a\"\ncomand = \"typo\"",
        "timeout = 3",
    ] {
        assert!(toml::from_str::<McpConfig>(bad).is_err(), "{bad}");
    }
}

#[test]
fn claude_json_servers_are_parsed_and_unsupported_ones_skipped() {
    let (servers, skipped) = parse_mcp_json(
        r#"{
          "mcpServers": {
            "fs": {"command": "npx", "args": ["-y", "fs"], "env": {"A": "1"}},
            "typed": {"type": "stdio", "command": "srv"},
            "web": {"type": "http", "url": "https://mcp.example.com/mcp", "headers": {"X-Key": "k"}},
            "old": {"type": "sse", "url": "https://sse.example.com/sse"},
            "broken": {"args": ["x"]}
          },
          "otherClaudeSetting": true
        }"#,
    )
    .unwrap();
    assert_eq!(
        servers.keys().collect::<Vec<_>>(),
        vec!["fs", "typed", "web"]
    );
    assert!(matches!(&servers["web"], ServerSpec::Http { headers, .. } if headers["X-Key"] == "k"));
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(
        skipped
            .iter()
            .any(|s| s.contains("old") && s.contains("sse"))
    );
    assert!(skipped.iter().any(|s| s.contains("broken")));

    let (none, _) = parse_mcp_json(r#"{"projects": {}}"#).unwrap();
    assert!(none.is_empty());
    assert!(parse_mcp_json("not json").is_err());
    assert!(parse_mcp_json(r#"{"mcpServers": []}"#).is_err());
}

#[test]
fn mcp_import_from_claude_json_preserves_comments() {
    let existing = "# my settings\nmodel = \"anthropic/claude-sonnet-5-5\" # keep this\n\n\
                    [mcp]\n# a note about mcp\ntimeout_s = 60\n\n\
                    [mcp.servers.fs]\ncommand = \"mine\"\n";
    let (servers, _) = parse_mcp_json(
        r#"{"mcpServers": {
            "fs": {"command": "theirs"},
            "git": {"command": "uvx", "args": ["mcp-git"], "env": {"GIT_DIR": ".git"}},
            "web": {"type": "http", "url": "https://mcp.example.com/mcp", "headers": {"Authorization": "Bearer x"}}
        }}"#,
    )
    .unwrap();
    let (out, kept) = import_into_toml(existing, &servers).unwrap();
    assert_eq!(kept, vec!["fs".to_string()]);
    assert!(
        out.starts_with("# my settings\nmodel = \"anthropic/claude-sonnet-5-5\" # keep this\n"),
        "{out}"
    );
    assert!(out.contains("# a note about mcp\ntimeout_s = 60"), "{out}");

    let parsed: toml::Table = toml::from_str(&out).unwrap();
    let mcp: McpConfig = parsed["mcp"].clone().try_into().unwrap();
    assert_eq!(mcp.timeout_s, 60);
    assert!(matches!(&mcp.servers["fs"], ServerSpec::Stdio { command, .. } if command == "mine"));
    assert_eq!(mcp.servers["git"], servers["git"]);
    assert_eq!(mcp.servers["web"], servers["web"]);

    // Into an empty file.
    let (fresh, kept) = import_into_toml("", &servers).unwrap();
    assert!(kept.is_empty());
    let parsed: toml::Table = toml::from_str(&fresh).unwrap();
    let mcp: McpConfig = parsed["mcp"].clone().try_into().unwrap();
    assert_eq!(mcp.servers, servers);

    assert!(import_into_toml("not = [toml", &servers).is_err());
}
