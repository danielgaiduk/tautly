//! The ACP bridge end to end (spec 7.11.9): a raw JSON-RPC client over an in-memory
//! duplex, the bridge, and engines on a scripted provider.

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{Value, json};
use tautly_acp::{AcpEngineFactory, Bridge, ModelChoices};
use tautly_core::{EngineError, Mode, ServerSpec};
use tautly_provider::{MockProvider, MockTurn, ProviderError, ProviderEvent, StopReason};

fn read(id: &str, path: &str) -> ProviderEvent {
    call(id, "read", json!({"path": path}))
}

fn bash(id: &str, command: &str) -> ProviderEvent {
    call(id, "bash", json!({"command": command}))
}

fn stop_reason(resp: &Value) -> &str {
    resp["result"]["stopReason"]
        .as_str()
        .unwrap_or_else(|| panic!("no stop reason: {resp}"))
}

/// The last `tool_call_update` for `id` that has a status.
fn final_update(acp: &Acp, id: &str) -> Value {
    acp.updates_of("tool_call_update")
        .into_iter()
        .rfind(|u| u["toolCallId"] == id && u.get("status").is_some())
        .unwrap_or_else(|| panic!("no update for {id}: {:?}", acp.updates()))
}

// --- initialize and sessions ---

#[tokio::test]
async fn initialize_returns_protocol_1_and_load_session() {
    let fx = Fx::new();
    let (f, _) = factory(&fx, config(Mode::Ask), vec![]);
    let mut acp = Acp::start(f);
    let init = acp.initialize(json!({})).await;
    assert_eq!(init["protocolVersion"], 1);
    let caps = &init["agentCapabilities"];
    assert_eq!(caps["loadSession"], true);
    assert_eq!(caps["promptCapabilities"]["embeddedContext"], true);
    assert_eq!(caps["promptCapabilities"]["image"], false);
    assert_eq!(caps["mcpCapabilities"]["http"], true);
    assert_eq!(init["authMethods"], json!([]));
    assert_eq!(init["agentInfo"]["name"], "tautly");
    acp.finish().await;
}

#[tokio::test]
async fn higher_client_version_gets_1() {
    let fx = Fx::new();
    let (f, _) = factory(&fx, config(Mode::Ask), vec![]);
    let mut acp = Acp::start(f);
    let r = acp
        .call(
            "initialize",
            json!({"protocolVersion": 7, "clientCapabilities": {}}),
        )
        .await;
    assert_eq!(r["result"]["protocolVersion"], 1, "{r}");
    // Methods before `initialize` are refused; so is a second `initialize`'s absence: here
    // the session methods work now.
    acp.finish().await;

    let (f, _) = factory(&fx, config(Mode::Ask), vec![]);
    let mut early = Acp::start(f);
    let r = early
        .call("session/new", json!({"cwd": fx.root(), "mcpServers": []}))
        .await;
    assert!(r.get("error").is_some(), "{r}");
    early.finish().await;
}

#[tokio::test]
async fn session_new_uses_git_top_level_of_cwd() {
    let fx = Fx::new();
    std::fs::create_dir_all(fx.dir.path().join(".git")).unwrap();
    fx.write("sub/dir/x.txt", "x");
    let (f, made) = factory(&fx, config(Mode::Ask), vec![]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({})).await;
    let id = acp.new_session(&fx.root().join("sub/dir"), json!([])).await;
    assert!(!id.is_empty());
    // Nothing of the session came before its response.
    assert!(acp.updates().is_empty(), "{:?}", acp.updates());
    assert_eq!(made.lock().unwrap().roots, vec![fx.root()]);
    // `cwd` must be absolute.
    let r = acp
        .call("session/new", json!({"cwd": "sub", "mcpServers": []}))
        .await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    // The advertised commands follow the response.
    let cmds = acp.updates_of("available_commands_update");
    assert_eq!(cmds.len(), 1, "{:?}", acp.updates());
    assert_eq!(cmds[0]["availableCommands"][0]["name"], "compact");
    acp.finish().await;
}

#[tokio::test]
async fn missing_key_fails_session_new_with_hint() {
    let f: AcpEngineFactory = Arc::new(|_, _, _| {
        Err(EngineError::Provider(
            "no API key for anthropic: set ANTHROPIC_API_KEY or store the key in the keychain\n\
             hint: store a key with `tautly auth add anthropic`, or set its environment variable"
                .into(),
        ))
    });
    let fx = Fx::new();
    let mut acp = Acp::start(f);
    acp.initialize(json!({})).await;
    let r = acp
        .call("session/new", json!({"cwd": fx.root(), "mcpServers": []}))
        .await;
    let err = &r["error"];
    assert!(err.is_object(), "{r}");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("ANTHROPIC_API_KEY"), "{message}");
    assert!(message.contains("tautly auth add anthropic"), "{message}");
    // No session: a prompt to any id is refused.
    let r = acp
        .call(
            "session/prompt",
            json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "hi"}]}),
        )
        .await;
    assert!(r.get("error").is_some(), "{r}");
    acp.finish().await;
}

#[tokio::test]
async fn two_sessions_share_one_workspace() {
    let fx = Fx::new();
    let (f, made) = factory(&fx, config(Mode::Ask), vec![]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({})).await;
    let a = acp.new_session(&fx.root(), json!([])).await;
    let b = acp.new_session(&fx.root(), json!([])).await;
    assert_ne!(a, b);
    {
        let m = made.lock().unwrap();
        assert_eq!(m.workspaces.len(), 2);
        assert!(Arc::ptr_eq(&m.workspaces[0], &m.workspaces[1]));
        assert_eq!(m.workspaces[0].live_sessions(), 2);
    }
    acp.finish().await;
}

#[tokio::test]
async fn client_mcp_servers_trusted_without_approval() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("hi")]);
    let (f, made) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({})).await;
    let missing = fx.root().join("no-such-server");
    let id = acp
        .new_session(
            &fx.root(),
            json!([{"name": "editor", "command": missing, "args": ["-x"], "env": [{"name": "K", "value": "v"}]}]),
        )
        .await;
    let resp = acp.prompt(&id, "hello").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    {
        let m = made.lock().unwrap();
        assert!(matches!(
            &m.mcp[0][..],
            [(name, ServerSpec::Stdio { args, env, .. })]
                if name == "editor" && args == &["-x"] && env["K"] == "v"
        ));
    }
    assert!(acp.permission_requests().is_empty());
    acp.finish().await;
}

#[tokio::test]
async fn session_load_replays_messages_and_tool_calls() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        text_step("it says alpha"),
    ]);
    let (f, made) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(Arc::clone(&f));
    let id = acp.open(&fx.root()).await;
    let resp = acp.prompt(&id, "what is in a.txt?").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    acp.finish().await;

    let mut again = Acp::start(f);
    again.initialize(json!({})).await;
    let r = again
        .call(
            "session/load",
            json!({"sessionId": id, "cwd": fx.root(), "mcpServers": []}),
        )
        .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(
        made.lock().unwrap().resumes[1].as_deref(),
        Some(id.as_str())
    );
    let kinds: Vec<String> = again
        .updates()
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap().to_string())
        .collect();
    let user = again.updates_of("user_message_chunk");
    assert_eq!(user[0]["content"]["text"], "what is in a.txt?", "{kinds:?}");
    let calls = again.updates_of("tool_call");
    assert_eq!(calls.len(), 1, "{kinds:?}");
    assert_eq!(calls[0]["toolCallId"], "r1");
    assert_eq!(calls[0]["status"], "completed");
    assert_eq!(calls[0]["kind"], "read");
    assert_eq!(calls[0]["title"], "read: `a.txt`");
    assert!(
        calls[0]["content"].to_string().contains("alpha"),
        "{}",
        calls[0]
    );
    assert_eq!(again.agent_text(), "it says alpha");
    let pos = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(pos("user_message_chunk") < pos("tool_call"));
    assert!(pos("tool_call") < pos("agent_message_chunk"));
    again.finish().await;
}

// --- prompts and streaming ---

#[tokio::test]
async fn prompt_streams_agent_message_chunks_and_ends_end_turn() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Events(vec![
        ProviderEvent::TextDelta("hel".into()),
        ProviderEvent::TextDelta("lo".into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock.clone()]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let resp = acp
        .call(
            "session/prompt",
            json!({"sessionId": id, "prompt": [
                {"type": "text", "text": "say"},
                {"type": "text", "text": "hello"},
            ]}),
        )
        .await;
    assert_eq!(stop_reason(&resp), "end_turn");
    assert_eq!(acp.agent_text(), "hello");
    let chunks = acp.updates_of("agent_message_chunk");
    assert_eq!(chunks[0]["content"]["type"], "text");
    assert!(
        acp.seen
            .iter()
            .all(|m| m["params"]["sessionId"] == json!(id) || m["method"] != "session/update")
    );
    // The text blocks were joined into one message.
    let req = &mock.requests()[0];
    assert!(format!("{req:?}").contains(r"say\nhello"), "{req:?}");
    acp.finish().await;
}

#[tokio::test]
async fn reasoning_maps_to_thought_chunks() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![MockTurn::Events(vec![
        ProviderEvent::ReasoningDelta("let me think".into()),
        ProviderEvent::TextDelta("answer".into()),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "q").await;
    let thoughts = acp.updates_of("agent_thought_chunk");
    assert_eq!(thoughts.len(), 1, "{:?}", acp.updates());
    assert_eq!(thoughts[0]["content"]["text"], "let me think");
    assert_eq!(acp.agent_text(), "answer");
    acp.finish().await;
}

#[tokio::test]
async fn tool_call_lifecycle_pending_in_progress_completed_with_kind_title_locations() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "read it").await;
    let call = &acp.updates_of("tool_call")[0];
    assert_eq!(call["toolCallId"], "r1");
    assert_eq!(call["status"], "pending");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["title"], "read: `a.txt`");
    assert_eq!(
        call["locations"],
        json!([{"path": fx.root().join("a.txt")}])
    );
    assert_eq!(call["rawInput"], json!({"path": "a.txt"}));
    let statuses: Vec<Value> = acp
        .updates_of("tool_call_update")
        .iter()
        .filter(|u| u["toolCallId"] == "r1")
        .map(|u| u["status"].clone())
        .collect();
    assert_eq!(statuses, vec![json!("in_progress"), json!("completed")]);
    let done = final_update(&acp, "r1");
    assert_eq!(done["content"][0]["type"], "content");
    assert!(
        done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("alpha")
    );
    acp.finish().await;
}

fn edit(id: &str, path: &str, old: &str, new: &str) -> ProviderEvent {
    call(
        id,
        "edit",
        json!({"path": path, "old_text": old, "new_text": new}),
    )
}

#[tokio::test]
async fn edit_result_carries_diff_content_old_new() {
    let fx = Fx::new();
    fx.write("src/lib.rs", "fn a() {}\nfn old() {}\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "src/lib.rs")]),
        tools_step(vec![edit("e1", "src/lib.rs", "fn old() {}", "fn new() {}")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::AutoEdit), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "rename").await;
    assert_eq!(fx.read("src/lib.rs"), "fn a() {}\nfn new() {}\n");
    let done = final_update(&acp, "e1");
    assert_eq!(done["status"], "completed");
    let diff = done["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "diff")
        .unwrap_or_else(|| panic!("no diff content: {done}"))
        .clone();
    assert_eq!(diff["path"], json!(fx.root().join("src/lib.rs")));
    assert_eq!(diff["oldText"], "fn a() {}\nfn old() {}\n");
    assert_eq!(diff["newText"], "fn a() {}\nfn new() {}\n");
    assert_eq!(acp.updates_of("tool_call")[1]["kind"], "edit");
    acp.finish().await;
}

#[tokio::test]
async fn oversized_file_falls_back_to_unified_text() {
    let fx = Fx::new();
    let big = format!("{}\nneedle\n", "filler line\n".repeat(30_000));
    assert!(big.len() > 256 * 1024);
    fx.write("big.txt", &big);
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "r1",
            "read",
            json!({"path": "big.txt", "start_line": 30000, "end_line": 30002}),
        )]),
        tools_step(vec![edit("e1", "big.txt", "needle", "pin")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::AutoEdit), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "edit big").await;
    let done = final_update(&acp, "e1");
    assert_eq!(done["status"], "completed", "{done}");
    let content = done["content"].as_array().unwrap();
    assert!(content.iter().all(|c| c["type"] != "diff"), "{done}");
    let text = content
        .iter()
        .map(|c| c["content"]["text"].as_str().unwrap_or("").to_string())
        .collect::<String>();
    assert!(text.contains("--- a/big.txt"), "{text}");
    assert!(text.contains("-needle\n+pin"), "{text}");
    acp.finish().await;
}

#[tokio::test]
async fn usage_update_has_used_size_and_usd_cost() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("hi")]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "q").await;
    let usage = acp.updates_of("usage_update");
    let last = usage
        .last()
        .unwrap_or_else(|| panic!("{:?}", acp.updates()));
    assert!(last["used"].as_u64().unwrap() > 0, "{last}");
    assert_eq!(last["size"], 200_000);
    assert_eq!(last["cost"]["currency"], "USD");
    // 100 input at $1/M and 10 output at $2/M.
    let amount = last["cost"]["amount"].as_f64().unwrap();
    assert!((amount - 0.00012).abs() < 1e-9, "{amount}");
    acp.finish().await;
}

#[tokio::test]
async fn slash_prompt_compact_maps_to_command() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock.clone()]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let resp = acp.prompt(&id, "/compact").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    // Nothing went to the model; the engine's answer is shown.
    assert!(mock.requests().is_empty());
    assert!(
        acp.agent_text().contains("nothing to compact"),
        "{:?}",
        acp.updates()
    );
    // `/diff` answers with the session's (empty) diff, `/cost` from the receipts.
    let resp = acp.prompt(&id, "/diff").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    let resp = acp.prompt(&id, "/cost").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    assert!(
        acp.agent_text().contains("no turn yet"),
        "{}",
        acp.agent_text()
    );
    acp.finish().await;
}

// --- permissions ---

#[tokio::test]
async fn ask_maps_to_request_permission_with_preview_diff() {
    let fx = Fx::new();
    fx.write("src/lib.rs", "fn a() {}\nfn old() {}\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "src/lib.rs")]),
        tools_step(vec![edit("e1", "src/lib.rs", "fn old() {}", "fn new() {}")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let mut asked = Vec::new();
    let resp = acp
        .prompt_answering(&id, "rename", |p| {
            asked.push(p.clone());
            Some(select("allow_once"))
        })
        .await;
    assert_eq!(stop_reason(&resp), "end_turn");
    assert_eq!(asked.len(), 1);
    let req = &asked[0];
    assert_eq!(req["sessionId"], json!(id));
    assert_eq!(req["toolCall"]["toolCallId"], "e1");
    assert_eq!(req["toolCall"]["title"], "edit: `src/lib.rs`");
    assert_eq!(req["toolCall"]["kind"], "edit");
    let options: Vec<(String, String)> = req["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["optionId"].as_str().unwrap().to_string(),
                o["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        options,
        [
            ("allow_once".to_string(), "allow_once".to_string()),
            ("allow_always".to_string(), "allow_always".to_string()),
            ("reject_once".to_string(), "reject_once".to_string()),
        ]
    );
    assert!(
        req["options"][1]["name"]
            .as_str()
            .unwrap()
            .contains("edit(src/lib.rs)"),
        "{req}"
    );
    let diff = &req["toolCall"]["content"][0];
    assert_eq!(diff["type"], "diff", "{req}");
    assert_eq!(diff["path"], json!(fx.root().join("src/lib.rs")));
    assert_eq!(diff["oldText"], "fn a() {}\nfn old() {}\n");
    assert_eq!(diff["newText"], "fn a() {}\nfn new() {}\n");
    assert_eq!(fx.read("src/lib.rs"), "fn a() {}\nfn new() {}\n");
    acp.finish().await;
}

#[tokio::test]
async fn allow_always_adds_session_rule() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("c1", "echo hi")]),
        tools_step(vec![bash("c2", "echo hi")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let mut asked = Vec::new();
    let resp = acp
        .prompt_answering(&id, "twice", |p| {
            asked.push(p.clone());
            Some(select("allow_always"))
        })
        .await;
    assert_eq!(stop_reason(&resp), "end_turn");
    assert_eq!(asked.len(), 1, "the second identical call is not asked");
    assert!(
        asked[0]["options"][1]["name"]
            .as_str()
            .unwrap()
            .contains("bash(echo hi)")
    );
    for c in ["c1", "c2"] {
        let done = final_update(&acp, c);
        assert_eq!(done["status"], "completed", "{done}");
    }
    acp.finish().await;
}

#[tokio::test]
async fn reject_or_cancelled_outcome_denies() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("c1", "touch one.txt")]),
        tools_step(vec![bash("c2", "touch two.txt")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let mut answers = vec![select("reject_once"), cancelled()].into_iter();
    let resp = acp.prompt_answering(&id, "touch", |_| answers.next()).await;
    assert_eq!(stop_reason(&resp), "end_turn");
    for c in ["c1", "c2"] {
        assert_eq!(final_update(&acp, c)["status"], "failed");
    }
    assert!(!fx.dir.path().join("one.txt").exists());
    assert!(!fx.dir.path().join("two.txt").exists());
    acp.finish().await;
}

#[tokio::test]
async fn memory_proposal_and_mcp_approval_use_request_permission() {
    let fx = Fx::new();
    fx.write(
        ".tautly/config.toml",
        "[mcp.servers.repo]\ncommand = \"/no/such/server\"\n",
    );
    let _ws = fx.ready_workspace().await;
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "m1",
            "remember",
            json!({"title": "Use pnpm", "kind": "convention", "body": "Run pnpm, never npm."}),
        )]),
        text_step("noted"),
    ]);
    let mut cfg = config(Mode::Ask);
    let loaded = tautly_core::Config::load_from(None, &fx.root(), Default::default()).unwrap();
    cfg.project_mcp_servers = loaded.config.project_mcp_servers;
    let (f, _) = factory(&fx, cfg, vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    // The repository's server asks before it starts.
    let mcp = acp.until_request("session/request_permission").await;
    let params = &mcp["params"];
    assert_eq!(params["sessionId"], json!(id));
    assert!(
        params["toolCall"]["title"]
            .as_str()
            .unwrap()
            .contains("repo"),
        "{params}"
    );
    let options: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(options, ["allow_once", "reject_once"]);
    acp.respond(&mcp["id"], select("reject_once")).await;
    // The memory proposal is a permission request too; approving it saves the entry.
    let resp = acp
        .prompt_answering(&id, "remember pnpm", |_| Some(select("allow_once")))
        .await;
    assert_eq!(stop_reason(&resp), "end_turn");
    let memory = acp.permission_requests();
    let memory = memory
        .iter()
        .find(|p| {
            p["toolCall"]["title"]
                .as_str()
                .unwrap_or("")
                .contains("Use pnpm")
        })
        .unwrap_or_else(|| panic!("no memory request: {:?}", acp.seen));
    let ids: Vec<&str> = memory["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["allow_once", "reject_once"]);
    let dir = fx.dir.path().join(".tautly/memory");
    let mut saved = false;
    for _ in 0..500 {
        if dir.join("use-pnpm.md").exists() {
            saved = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(saved, "the approved memory entry was not written");
    acp.finish().await;
}

// --- cancel, stop reasons, model ---

#[tokio::test]
async fn cancel_answers_pending_permission_and_returns_cancelled() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("c1", "touch one.txt")]),
        text_step("never"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let prompt = acp
        .send(
            "session/prompt",
            json!({"sessionId": id, "prompt": [{"type": "text", "text": "go"}]}),
        )
        .await;
    let asked = acp.until_request("session/request_permission").await;
    acp.notify("session/cancel", json!({"sessionId": id})).await;
    // The permission request is left unanswered: the bridge denies it itself.
    let resp = acp.response_answering(prompt, |_| None).await;
    assert_eq!(stop_reason(&resp), "cancelled", "{resp}");
    assert!(!fx.dir.path().join("one.txt").exists());
    // A late answer is ignored.
    acp.respond(&asked["id"], select("allow_once")).await;
    let r = acp.prompt(&id, "/cost").await;
    assert_eq!(stop_reason(&r), "end_turn");
    assert!(!fx.dir.path().join("one.txt").exists());
    acp.finish().await;
}

#[tokio::test]
async fn stop_reason_mapping() {
    // max_turns → max_turn_requests.
    let fx = Fx::new();
    fx.write("a.txt", "a");
    let mut cfg = config(Mode::Ask);
    cfg.max_turns = 1;
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        tools_step(vec![read("r2", "a.txt")]),
        text_step("never"),
    ]);
    let (f, _) = factory(&fx, cfg, vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    assert_eq!(
        stop_reason(&acp.prompt(&id, "go").await),
        "max_turn_requests"
    );
    acp.finish().await;

    // budget → end_turn, after a notice for a client that shows them.
    let mut cfg = config(Mode::Ask);
    cfg.budget.turn.max_tokens = Some(100);
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        text_step("never"),
    ]);
    let (f, _) = factory(&fx, cfg, vec![mock]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({"session": {"notices": {}}})).await;
    let id = acp.new_session(&fx.root(), json!([])).await;
    assert_eq!(stop_reason(&acp.prompt(&id, "go").await), "end_turn");
    let notices = acp.updates_of("notice");
    assert!(
        notices
            .iter()
            .any(|n| n["title"].as_str().unwrap().contains("budget")),
        "{notices:?}"
    );
    // The receipt is a notice too.
    assert!(
        notices
            .iter()
            .any(|n| n["title"].as_str().unwrap().starts_with("turn 1")),
        "{notices:?}"
    );
    acp.finish().await;

    // Without `session.notices` no notice is sent.
    let mut cfg = config(Mode::Ask);
    cfg.budget.turn.max_tokens = Some(100);
    let mock = MockProvider::new(vec![tools_step(vec![read("r1", "a.txt")])]);
    let (f, _) = factory(&fx, cfg, vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    assert_eq!(stop_reason(&acp.prompt(&id, "go").await), "end_turn");
    assert!(acp.updates_of("notice").is_empty());
    assert!(acp.agent_text().contains("budget"), "{}", acp.agent_text());
    acp.finish().await;

    // error → a JSON-RPC error.
    let mock = MockProvider::new(vec![MockTurn::Error(ProviderError::Auth("bad key".into()))]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let r = acp.prompt(&id, "go").await;
    assert!(r.get("result").is_none(), "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("bad key"),
        "{r}"
    );
    acp.finish().await;
}

#[tokio::test]
async fn set_config_option_model_sends_set_model_and_returns_options() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![text_step("hi")]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let lister = Arc::new(|_: &std::path::Path| {
        Some(ModelChoices {
            current: MODEL.to_string(),
            available: vec![MODEL.to_string(), OTHER_MODEL.to_string()],
        })
    });
    let mut acp = Acp::with(Bridge::new(f).models(lister));
    acp.initialize(json!({})).await;
    let r = acp
        .call("session/new", json!({"cwd": fx.root(), "mcpServers": []}))
        .await;
    let id = r["result"]["sessionId"].as_str().unwrap().to_string();
    let opts = &r["result"]["configOptions"];
    assert_eq!(opts[0]["id"], "model", "{r}");
    assert_eq!(opts[0]["category"], "model");
    assert_eq!(opts[0]["type"], "select");
    assert_eq!(opts[0]["currentValue"], MODEL);
    let values: Vec<&str> = opts[0]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, [MODEL, OTHER_MODEL]);

    let r = acp
        .call(
            "session/set_config_option",
            json!({"sessionId": id, "configId": "model", "value": OTHER_MODEL}),
        )
        .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["result"]["configOptions"][0]["currentValue"], OTHER_MODEL);
    let resp = acp.prompt(&id, "hi").await;
    assert_eq!(stop_reason(&resp), "end_turn");

    let r = acp
        .call(
            "session/set_config_option",
            json!({"sessionId": id, "configId": "mode", "value": "x"}),
        )
        .await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    acp.finish().await;
}

// --- review fixes (Task 46 fix round 1) ---

/// I1: approval text is escaped (control and bidi characters as visible escapes) and the
/// command is fenced with more backticks than any run in it, so markdown in it shows as
/// text.
#[tokio::test]
async fn approval_text_is_escaped_and_fenced() {
    let fx = Fx::new();
    let command = "echo ```x``` <!-- hidden --> [a](b)\u{1b}[2K\rshown\u{202e}evil";
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("c1", command)]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    let mut asked = Vec::new();
    acp.prompt_answering(&id, "go", |p| {
        asked.push(p.clone());
        Some(select("reject_once"))
    })
    .await;
    let req = &asked[0];
    let text = req["toolCall"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        text.starts_with("````\n") && text.ends_with("\n````"),
        "{text}"
    );
    assert!(
        text.contains(r"<!-- hidden --> [a](b)\u{1b}[2K\rshown\u{202e}evil"),
        "{text}"
    );
    let title = req["toolCall"]["title"].as_str().unwrap();
    for t in [text, title] {
        assert!(
            !t.chars()
                .any(|c| c == '\u{1b}' || c == '\r' || c == '\u{202e}'),
            "{t:?}"
        );
    }
    assert!(title.starts_with("bash: ````echo ```x```"), "{title}");
    assert!(title.ends_with("````"), "{title}");
    // The tool call's title is escaped the same way.
    let call = &acp.updates_of("tool_call")[0];
    assert_eq!(call["title"], req["toolCall"]["title"]);
    acp.finish().await;
}

/// I2: `/compact` answers once the compaction is over: after the summary the side model
/// wrote, with its result.
#[tokio::test]
async fn compact_runs_a_real_summary_before_answering() {
    let fx = Fx::new();
    for i in 1..=4 {
        fx.write(&format!("f{i}.txt"), &format!("file {i}\n"));
    }
    let mut cfg = config(Mode::Ask);
    cfg.catalog_overrides = Some(catalog_window(16_000, 1_000));
    let mut script: Vec<MockTurn> = (1..=4)
        .map(|i| tools_step(vec![read(&format!("r{i}"), &format!("f{i}.txt"))]))
        .collect();
    script.push(text_step("first done"));
    script.push(text_step("SUMMARY-ONE"));
    let mock = MockProvider::new(script);
    let (f, _) = factory(&fx, cfg, vec![mock.clone()]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "read them").await;
    let before = mock.requests().len();
    let resp = acp.prompt(&id, "/compact").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    // The summary request was made before the answer, and its result is in it.
    assert_eq!(mock.requests().len(), before + 1);
    let text = acp.agent_text();
    assert!(text.contains("compacted"), "{text}");
    assert!(text.contains("tokens freed"), "{text}");
    // Nothing left to compact: the warning is the answer.
    let resp = acp.prompt(&id, "/compact").await;
    assert_eq!(stop_reason(&resp), "end_turn");
    assert!(acp.agent_text().contains("nothing to compact"));
    acp.finish().await;
}

#[tokio::test]
async fn two_sessions_prompt_at_once() {
    let fx = Fx::new();
    let (f, _) = factory(
        &fx,
        config(Mode::Ask),
        vec![
            MockProvider::new(vec![text_step("from A")]),
            MockProvider::new(vec![text_step("from B")]),
        ],
    );
    let mut acp = Acp::start(f);
    let a = acp.open(&fx.root()).await;
    let b = acp.new_session(&fx.root(), json!([])).await;
    let pa = acp
        .send(
            "session/prompt",
            json!({"sessionId": a, "prompt": [{"type": "text", "text": "a"}]}),
        )
        .await;
    let pb = acp
        .send(
            "session/prompt",
            json!({"sessionId": b, "prompt": [{"type": "text", "text": "b"}]}),
        )
        .await;
    let ra = acp.response(pa).await;
    let rb = acp.response(pb).await;
    assert_eq!(stop_reason(&ra), "end_turn");
    assert_eq!(stop_reason(&rb), "end_turn");
    let said = |id: &str| -> String {
        updates_for(&acp, id)
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .map(|u| u["content"]["text"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(said(&a), "from A");
    assert_eq!(said(&b), "from B");
    acp.finish().await;
}

#[tokio::test]
async fn eof_with_a_pending_permission_denies_and_exits() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("c1", "touch one.txt")]),
        text_step("never"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.send(
        "session/prompt",
        json!({"sessionId": id, "prompt": [{"type": "text", "text": "go"}]}),
    )
    .await;
    acp.until_request("session/request_permission").await;
    // The editor goes away without answering: the bridge still ends, the call denied.
    acp.finish().await;
    assert!(!fx.dir.path().join("one.txt").exists());
}

#[tokio::test]
async fn session_load_replays_edit_diffs_and_cost() {
    let fx = Fx::new();
    fx.write("a.txt", "old\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        tools_step(vec![edit("e1", "a.txt", "old", "new")]),
        text_step("edited"),
    ]);
    let (f, _) = factory(&fx, config(Mode::AutoEdit), vec![mock]);
    let mut acp = Acp::start(Arc::clone(&f));
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "edit").await;
    acp.finish().await;

    let mut again = Acp::with(Bridge::new(f).data_home(fx.home.path()));
    again.initialize(json!({})).await;
    let r = again
        .call(
            "session/load",
            json!({"sessionId": id, "cwd": fx.root(), "mcpServers": []}),
        )
        .await;
    assert!(r.get("error").is_none(), "{r}");
    let calls = again.updates_of("tool_call");
    let e1 = calls.iter().find(|c| c["toolCallId"] == "e1").unwrap();
    let diff = e1["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "diff")
        .unwrap_or_else(|| panic!("{e1}"));
    assert_eq!(diff["oldText"], "old\n");
    assert_eq!(diff["newText"], "new\n");
    // The session's earlier spend carries on (three steps of 100 in, 10 out at $1/$2 per M).
    again.prompt(&id, "/cost").await;
    assert!(
        again.agent_text().contains("session: $0.0004"),
        "{}",
        again.agent_text()
    );
    again.finish().await;
}

/// M1: only an object advertises notices.
#[tokio::test]
async fn notices_false_is_no_notice_support() {
    let fx = Fx::new();
    fx.write("a.txt", "a");
    let mut cfg = config(Mode::Ask);
    cfg.budget.turn.max_tokens = Some(100);
    let mock = MockProvider::new(vec![tools_step(vec![read("r1", "a.txt")])]);
    let (f, _) = factory(&fx, cfg, vec![mock]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({"session": {"notices": false}})).await;
    let id = acp.new_session(&fx.root(), json!([])).await;
    assert_eq!(stop_reason(&acp.prompt(&id, "go").await), "end_turn");
    assert!(acp.updates_of("notice").is_empty());
    assert!(acp.agent_text().contains("budget"));
    acp.finish().await;
}

/// M4: a switch asked for during a turn answers at once; when it then fails, the real
/// model is put back with a `config_option_update`.
#[tokio::test]
async fn set_config_option_during_a_turn_reverts_when_the_switch_fails() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![bash("c1", "echo hi")]),
        text_step("done"),
    ]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock]);
    let lister = Arc::new(|_: &std::path::Path| {
        Some(ModelChoices {
            current: MODEL.to_string(),
            available: vec![MODEL.to_string(), "mock/unknown".to_string()],
        })
    });
    let mut acp = Acp::with(Bridge::new(f).models(lister));
    let id = acp.open(&fx.root()).await;
    let prompt = acp
        .send(
            "session/prompt",
            json!({"sessionId": id, "prompt": [{"type": "text", "text": "go"}]}),
        )
        .await;
    let asked = acp.until_request("session/request_permission").await;
    // Busy: answered at once with the model asked for.
    let r = acp
        .call(
            "session/set_config_option",
            json!({"sessionId": id, "configId": "model", "value": "mock/unknown"}),
        )
        .await;
    assert_eq!(
        r["result"]["configOptions"][0]["currentValue"], "mock/unknown",
        "{r}"
    );
    acp.respond(&asked["id"], select("allow_once")).await;
    assert_eq!(stop_reason(&acp.response(prompt).await), "end_turn");
    // The switch runs after the turn and fails (no such provider): the real model is back.
    acp.prompt(&id, "/diff").await;
    let updates = acp.updates_of("config_option_update");
    assert_eq!(
        updates.last().unwrap()["configOptions"][0]["currentValue"],
        MODEL,
        "{updates:?}"
    );
    acp.finish().await;
}

/// M9: `/undo` shows its dry run for approval, then undoes.
#[tokio::test]
async fn undo_asks_with_the_dry_run_first() {
    let fx = Fx::new();
    fx.write("a.txt", "old\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![read("r1", "a.txt")]),
        tools_step(vec![edit("e1", "a.txt", "old", "new")]),
        text_step("edited"),
    ]);
    let (f, _) = factory(&fx, config(Mode::AutoEdit), vec![mock]);
    let mut acp = Acp::start(f);
    let id = acp.open(&fx.root()).await;
    acp.prompt(&id, "edit").await;
    assert_eq!(fx.read("a.txt"), "new\n");
    let mut asked = Vec::new();
    let resp = acp
        .prompt_answering(&id, "/undo", |p| {
            asked.push(p.clone());
            Some(select("allow_once"))
        })
        .await;
    assert_eq!(stop_reason(&resp), "end_turn");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["toolCall"]["title"], "Undo turn 1");
    assert!(
        asked[0]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("restore a.txt"),
        "{}",
        asked[0]
    );
    assert_eq!(fx.read("a.txt"), "old\n");
    assert!(
        acp.agent_text().contains("undid turn 1"),
        "{}",
        acp.agent_text()
    );
    acp.finish().await;
}

/// M3: a cancel leaves a repository MCP server's approval open; answering it later still
/// counts.
#[tokio::test]
async fn cancel_leaves_a_pending_mcp_approval_open() {
    let fx = Fx::new();
    fx.write(
        ".tautly/config.toml",
        "[mcp.servers.repo]\ncommand = \"/no/such/server\"\n",
    );
    let mut cfg = config(Mode::Ask);
    let loaded = tautly_core::Config::load_from(None, &fx.root(), Default::default()).unwrap();
    cfg.project_mcp_servers = loaded.config.project_mcp_servers;
    let (f, _) = factory(&fx, cfg, vec![]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({"session": {"notices": {}}})).await;
    let id = acp.new_session(&fx.root(), json!([])).await;
    let mcp = acp.until_request("session/request_permission").await;
    acp.notify("session/cancel", json!({"sessionId": id})).await;
    acp.respond(&mcp["id"], select("allow_once")).await;
    // Approved: the server is started (and fails: no such command).
    loop {
        let msg = acp.next().await;
        let title = msg["params"]["update"]["title"].as_str().unwrap_or("");
        if title.contains("MCP server `repo` failed") {
            break;
        }
    }
    acp.finish().await;
}

/// M10: an editor's MCP server starts trusted (no approval), its tools are offered, and
/// its process is gone once the editor disconnects.
#[tokio::test]
async fn client_mcp_server_starts_trusted_and_is_reaped_at_eof() {
    let fx = Fx::new();
    let pid_file = fx.home.path().join("pids");
    let mock = MockProvider::new(vec![text_step("hi")]);
    let (f, _) = factory(&fx, config(Mode::Ask), vec![mock.clone()]);
    let mut acp = Acp::start(f);
    acp.initialize(json!({"session": {"notices": {}}})).await;
    let id = acp
        .new_session(
            &fx.root(),
            json!([{"name": "editor", "command": mcp_fixture(), "args": [],
                    "env": [{"name": "ECHO_PID_FILE", "value": pid_file}]}]),
        )
        .await;
    loop {
        let msg = acp.next().await;
        let title = msg["params"]["update"]["title"].as_str().unwrap_or("");
        assert!(!title.contains("failed"), "{msg}");
        if title.contains("MCP server `editor` ready") {
            break;
        }
    }
    assert_eq!(stop_reason(&acp.prompt(&id, "hello").await), "end_turn");
    assert!(acp.permission_requests().is_empty());
    assert!(
        format!("{:?}", mock.requests()[0]).contains("mcp__editor__echo"),
        "the editor's tools are offered"
    );
    let pids: Vec<u32> = std::fs::read_to_string(&pid_file)
        .unwrap()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(!pids.is_empty() && pids.iter().all(|p| alive(*p)));
    acp.finish().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while pids.iter().any(|p| alive(*p)) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!pids.iter().any(|p| alive(*p)), "left running: {pids:?}");
}
