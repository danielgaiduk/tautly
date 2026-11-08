//! Fixtures for the bridge tests: a scripted provider, a temporary work tree and data
//! home, an engine factory, and an ACP client speaking raw JSON-RPC over an in-memory
//! duplex.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tautly_acp::{AcpEngineFactory, AcpError, Bridge, shared_factory};
use tautly_core::{
    Config, EngineBuilder, Mode, ModelId, ProviderFactory, ServerSpec, Workspace, Workspaces,
};
use tautly_provider::{
    Catalog, MockProvider, MockTurn, Provider, ProviderEvent, RawUsage, StopReason,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub const MODEL: &str = "mock/test";
pub const OTHER_MODEL: &str = "openai/test-other";
pub const WAIT: Duration = Duration::from_secs(30);

pub fn catalog() -> Catalog {
    catalog_window(200_000, 8000)
}

/// The mock models with a `window`-token context and `max_output` (a small window makes
/// a compaction summarize).
pub fn catalog_window(window: u32, max_output: u32) -> Catalog {
    let entry = |id: &str| {
        format!(
            r#"[models."{id}"]
context_window = {window}
max_output = {max_output}
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
[models."{id}".price]
input = 1.0
output = 2.0
cache_read = 0.0
cache_write = 0.0
"#
        )
    };
    Catalog::from_toml(&format!("{}{}", entry(MODEL), entry(OTHER_MODEL))).unwrap()
}

pub fn config(mode: Mode) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog()),
        ..Config::default()
    };
    c.permissions.mode = mode;
    c.checkpoints.enabled = false;
    c
}

pub fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        input_json: args.to_string(),
    }
}

fn usage() -> ProviderEvent {
    ProviderEvent::Usage(RawUsage {
        input: 100,
        output: 10,
        cache_read: 0,
        cache_write: 0,
    })
}

pub fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(usage());
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

pub fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        usage(),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

pub struct Fx {
    pub dir: TempDir,
    pub home: TempDir,
    pub workspaces: Workspaces,
}

impl Fx {
    pub fn new() -> Fx {
        let home = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::new(home.path());
        Fx {
            dir: tempfile::tempdir().unwrap(),
            home,
            workspaces,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    pub fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(rel)).unwrap()
    }

    /// The root's workspace, opened (index and memory store) and held by the caller so
    /// the sessions get it.
    pub async fn ready_workspace(&self) -> Arc<Workspace> {
        let ws = self.workspaces.get(&self.root()).unwrap();
        ws.start();
        assert!(ws.wait_ready_or_failed().await, "index never became ready");
        for _ in 0..1000 {
            if ws.memory().is_some() {
                return ws;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("memory never opened");
    }
}

/// What the factory was asked for.
#[derive(Default)]
pub struct Made {
    pub roots: Vec<PathBuf>,
    pub resumes: Vec<Option<String>>,
    pub mcp: Vec<Vec<(String, ServerSpec)>>,
    pub workspaces: Vec<Arc<Workspace>>,
}

/// A factory building each session on the next of `mocks` (an empty script once they run
/// out) with `cfg`, interactive, on the fixture's shared workspaces. `SetModel` builds the
/// mock again.
pub fn factory(
    fx: &Fx,
    cfg: Config,
    mocks: Vec<MockProvider>,
) -> (AcpEngineFactory, Arc<Mutex<Made>>) {
    let made = Arc::new(Mutex::new(Made::default()));
    let mocks = Arc::new(Mutex::new(VecDeque::from(mocks)));
    let home = fx.home.path().to_path_buf();
    let record = Arc::clone(&made);
    let f = shared_factory(fx.workspaces.clone(), move |root, resume, mcp, ws| {
        let mock = mocks
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| MockProvider::new(vec![]));
        {
            let mut m = record.lock().unwrap();
            m.roots.push(root.clone());
            m.resumes.push(resume.clone());
            m.mcp.push(mcp.clone());
            m.workspaces.push(Arc::clone(&ws));
        }
        let provider: Arc<dyn Provider> = Arc::new(mock);
        let again = Arc::clone(&provider);
        let provider_factory: ProviderFactory =
            Arc::new(move |_model: &ModelId| Ok(Arc::clone(&again)));
        EngineBuilder {
            provider: Some(provider),
            provider_factory: Some(provider_factory),
            data_home: Some(home.clone()),
            workspace: Some(ws),
            interactive: true,
            resume,
            client_mcp: mcp,
            ..EngineBuilder::new(cfg.clone(), root)
        }
        .build()
    });
    (f, made)
}

/// An ACP client on one end of a duplex, the bridge serving the other.
pub struct Acp {
    write: WriteHalf<DuplexStream>,
    incoming: mpsc::UnboundedReceiver<Value>,
    next_id: u64,
    /// Every message from the agent read so far that was not a response a call waited
    /// for: notifications and the agent's requests, in order.
    pub seen: Vec<Value>,
    pub serve: JoinHandle<Result<(), AcpError>>,
}

impl Acp {
    pub fn start(factory: AcpEngineFactory) -> Acp {
        Acp::with(Bridge::new(factory))
    }

    pub fn with(bridge: Bridge) -> Acp {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (their_read, their_write) = tokio::io::split(theirs);
        let serve = tokio::spawn(bridge.serve(their_read, their_write));
        let (read, write) = tokio::io::split(ours);
        let (tx, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| {
                    panic!("the agent wrote a line that is not JSON ({e}): {line}")
                });
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Acp {
            write,
            incoming,
            next_id: 1,
            seen: Vec::new(),
            serve,
        }
    }

    async fn write(&mut self, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
    }

    /// Send a request; its id.
    pub async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// Answer the agent's request `id`.
    pub async fn respond(&mut self, id: &Value, result: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    /// The next message from the agent.
    pub async fn next(&mut self) -> Value {
        tokio::time::timeout(WAIT, self.incoming.recv())
            .await
            .expect("timed out waiting for the agent")
            .expect("the agent closed its output")
    }

    /// Read until the response to request `id` (returned whole); everything else goes to
    /// `seen`, and the agent's permission requests are answered by `answer` (given their
    /// params; None leaves the request unanswered).
    pub async fn response_answering(
        &mut self,
        id: u64,
        mut answer: impl FnMut(&Value) -> Option<Value>,
    ) -> Value {
        // It may have come while waiting for another response.
        if let Some(i) = self
            .seen
            .iter()
            .position(|m| m.get("method").is_none() && m["id"] == json!(id))
        {
            return self.seen.remove(i);
        }
        loop {
            let msg = self.next().await;
            if msg.get("method").is_none() && msg["id"] == json!(id) {
                return msg;
            }
            if msg["method"] == "session/request_permission"
                && let Some(result) = answer(&msg["params"])
            {
                let rid = msg["id"].clone();
                self.respond(&rid, result).await;
            }
            self.seen.push(msg);
        }
    }

    /// [`response_answering`](Self::response_answering) that fails on a permission request.
    pub async fn response(&mut self, id: u64) -> Value {
        self.response_answering(id, |p| panic!("unexpected permission request: {p}"))
            .await
    }

    /// Send a request and wait for its response.
    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }

    /// Read until the agent's next request for `method`; earlier messages go to `seen`.
    pub async fn until_request(&mut self, method: &str) -> Value {
        loop {
            let msg = self.next().await;
            if msg["method"] == method && msg.get("id").is_some() {
                return msg;
            }
            self.seen.push(msg);
        }
    }

    pub async fn initialize(&mut self, caps: Value) -> Value {
        let r = self
            .call(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": caps}),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        r["result"].clone()
    }

    /// `initialize` then `session/new` on `cwd`; the session id.
    pub async fn open(&mut self, cwd: &Path) -> String {
        self.initialize(json!({})).await;
        self.new_session(cwd, json!([])).await
    }

    pub async fn new_session(&mut self, cwd: &Path, mcp: Value) -> String {
        let r = self
            .call("session/new", json!({"cwd": cwd, "mcpServers": mcp}))
            .await;
        assert!(r.get("error").is_none(), "{r}");
        r["result"]["sessionId"].as_str().unwrap().to_string()
    }

    /// `session/prompt` with one text block, answering permission requests with `answer`;
    /// the whole response.
    pub async fn prompt_answering(
        &mut self,
        session: &str,
        text: &str,
        answer: impl FnMut(&Value) -> Option<Value>,
    ) -> Value {
        let id = self
            .send(
                "session/prompt",
                json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
            )
            .await;
        self.response_answering(id, answer).await
    }

    pub async fn prompt(&mut self, session: &str, text: &str) -> Value {
        self.prompt_answering(session, text, |p| {
            panic!("unexpected permission request: {p}")
        })
        .await
    }

    /// The `update`s of the `session/update` notifications seen so far.
    pub fn updates(&self) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|m| m["method"] == "session/update")
            .map(|m| m["params"]["update"].clone())
            .collect()
    }

    /// The updates seen so far whose `sessionUpdate` is `kind`.
    pub fn updates_of(&self, kind: &str) -> Vec<Value> {
        self.updates()
            .into_iter()
            .filter(|u| u["sessionUpdate"] == kind)
            .collect()
    }

    /// The agent's permission requests seen so far.
    pub fn permission_requests(&self) -> Vec<Value> {
        self.seen
            .iter()
            .filter(|m| m["method"] == "session/request_permission")
            .map(|m| m["params"].clone())
            .collect()
    }

    /// The text of the `agent_message_chunk`s seen so far.
    pub fn agent_text(&self) -> String {
        self.updates_of("agent_message_chunk")
            .iter()
            .map(|u| u["content"]["text"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// End the input and wait for the bridge to finish (every session shut down).
    pub async fn finish(mut self) {
        self.write.shutdown().await.unwrap();
        let served = tokio::time::timeout(WAIT, &mut self.serve)
            .await
            .expect("the bridge did not finish")
            .unwrap();
        served.unwrap();
    }
}

/// The `echo_server` MCP fixture, built once.
pub fn mcp_fixture() -> String {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().parent().unwrap();
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "tautly-mcp-fixture", "--bin", "echo_server"])
            .status()
            .unwrap();
        assert!(status.success(), "could not build the echo_server fixture");
        let bin = dir.join("echo_server");
        assert!(bin.exists(), "no fixture at {}", bin.display());
        bin.to_string_lossy().into_owned()
    })
    .clone()
}

/// Whether process `pid` is alive.
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The updates seen for session `id`.
pub fn updates_for(acp: &Acp, id: &str) -> Vec<Value> {
    acp.seen
        .iter()
        .filter(|m| m["method"] == "session/update" && m["params"]["sessionId"] == id)
        .map(|m| m["params"]["update"].clone())
        .collect()
}

/// A permission result selecting `option`.
pub fn select(option: &str) -> Value {
    json!({"outcome": {"outcome": "selected", "optionId": option}})
}

pub fn cancelled() -> Value {
    json!({"outcome": {"outcome": "cancelled"}})
}
