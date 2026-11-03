//! `[mcp]` parsing, Claude-style JSON and the import into a user `config.toml` (the
//! library side of `tautly mcp import`).

use std::collections::BTreeMap;

use tautly_mcp::config::{import_into_toml, parse_mcp_json};
use tautly_mcp::{McpConfig, ServerSource, ServerSpec, printable};

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
        cfg.servers["files"].launch(ServerSource::User),
        "stdio: npx -y fs (env: ROOT)"
    );
    assert_eq!(
        cfg.servers["remote"].launch(ServerSource::User),
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

#[test]
fn launch_shows_repository_values_escaped_and_cut() {
    let spec = ServerSpec::Stdio {
        command: "node".into(),
        args: vec!["\r\u{1b}[2Kok".into(), "a b".into()],
        env: BTreeMap::from([
            ("NODE_OPTIONS".into(), "--require ./evil.js".into()),
            ("LONG".into(), "x".repeat(500)),
        ]),
        cwd: Some("sub\u{202e}dir".into()),
    };
    let user = spec.launch(ServerSource::User);
    assert!(!user.contains("evil"), "{user}");
    assert!(user.contains("(env: LONG, NODE_OPTIONS)"), "{user}");
    for source in [ServerSource::Project, ServerSource::McpJson] {
        let repo = spec.launch(source);
        assert!(repo.contains("NODE_OPTIONS=--require ./evil.js"), "{repo}");
        // A long value shows its head, its tail and its length.
        assert!(
            repo.contains(&format!(
                "LONG={}…{} (500 bytes)",
                "x".repeat(120),
                "x".repeat(60)
            )),
            "{repo}"
        );
        assert!(!repo.contains(&"x".repeat(121)), "{repo}");
        // Nothing a terminal would act on gets through.
        assert!(
            !repo.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "{repo:?}"
        );
        assert!(repo.contains("\\r\\u{1b}[2Kok"), "{repo}");
        assert!(repo.contains("sub\\u{202e}dir"), "{repo}");
    }
    let http = ServerSpec::Http {
        url: "https://mcp.example.com/mcp".parse().unwrap(),
        headers: BTreeMap::from([("X-Target".into(), "prod\u{7}".into())]),
    };
    assert_eq!(
        http.launch(ServerSource::Project),
        "http: https://mcp.example.com/mcp (headers: X-Target=prod\\u{7})"
    );
    assert_eq!(
        http.launch(ServerSource::User),
        "http: https://mcp.example.com/mcp (headers: X-Target)"
    );
}

#[test]
fn printable_escapes_control_and_invisible_characters() {
    assert_eq!(printable("plain text, ü ok"), "plain text, ü ok");
    assert_eq!(printable("a\u{1b}[31mb"), "a\\u{1b}[31mb");
    assert_eq!(printable("x\r\ny\t"), "x\\r\\ny\\t");
    for c in [
        '\u{202e}', '\u{2066}', '\u{200b}', '\u{feff}', '\u{85}', '\u{2028}',
    ] {
        let out = printable(&format!("a{c}b"));
        assert!(!out.contains(c), "{out:?}");
        assert!(out.contains("\\u{"), "{out:?}");
    }
}

#[test]
fn long_values_show_what_follows_padding() {
    let padded = format!(
        "{}--require ./evil.js",
        "--max-old-space-size=4096 ".repeat(8)
    );
    let spec = ServerSpec::Stdio {
        command: "node".into(),
        args: vec![],
        env: BTreeMap::from([("NODE_OPTIONS".into(), padded.clone())]),
        cwd: None,
    };
    let shown = spec.launch(ServerSource::Project);
    assert!(shown.contains("--require ./evil.js"), "{shown}");
    assert!(
        shown.contains(&format!("({} bytes)", padded.len())),
        "{shown}"
    );
}

#[test]
fn printable_escapes_backslashes_and_every_invisible_character() {
    // A literal backslash-r is not an escaped carriage return.
    assert_eq!(printable("a\\rb"), "a\\\\rb");
    assert_ne!(printable("a\\rb"), printable("a\rb"));
    for c in [
        '\u{e0041}', // tag characters (ASCII smuggling)
        '\u{e007f}',
        '\u{e0001}',
        '\u{206a}',
        '\u{206f}',
        '\u{115f}', // Hangul fillers
        '\u{1160}',
        '\u{3164}',
        '\u{ffa0}',
        '\u{fe00}', // variation selectors
        '\u{fe0f}',
        '\u{e0100}',
        '\u{e01ef}',
        '\u{34f}',
        '\u{600}', // other format (Cf) characters
        '\u{6dd}',
        '\u{180e}',
        '\u{1d173}',
        '\u{13430}',
    ] {
        let out = printable(&format!("a{c}b"));
        assert!(!out.contains(c), "{c:?} -> {out:?}");
    }
    assert_eq!(
        printable("plain ü, 日本, emoji 🙂"),
        "plain ü, 日本, emoji 🙂"
    );
}
