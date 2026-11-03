//! A test MCP server for the `tautly-mcp` and `tautly-core` tests.
//!
//! `echo_server [stdio|http|legacy|hang] [URL_FILE]`:
//! - `stdio` (default): an `rmcp` server on stdin/stdout.
//! - `http`: an `rmcp` streamable-HTTP server on 127.0.0.1 (a free port); the endpoint URL
//!   is written to `URL_FILE`.
//! - `legacy`: a hand-written server that knows only the `initialize` handshake of
//!   2025-06-18 and answers `server/discover` with "method not found", like servers built
//!   before 2026-07-28.
//! - `hang`: writes the pid file, then never answers (a server stuck in its handshake).
//!
//! Tools: `echo {text}`, `env` (its environment, `NAME=VALUE` per line), `slow {ms}`,
//! `mixed` (text, image, resource link), `structured` (only `structuredContent`), `big
//! {bytes}`, `fail` (an error result) and `change_tools` (adds `extra` and sends
//! `notifications/tools/list_changed`), `storm` (200 such notifications), `exit` (answers,
//! then the process exits with status 3) and, over HTTP, `headers` (the request's headers,
//! `name: value` per line).
//!
//! Environment: `ECHO_FILLER=N` adds tools `filler_000`..; `ECHO_BIG_SCHEMA=1` adds
//! `big_schema` (a 9 KB input schema); `ECHO_LONG_DESC=1` adds `long_desc` (a 3 KB
//! description); `ECHO_START_DELAY_MS` delays start-up; `ECHO_PID_FILE` receives this
//! process's pid and that of a `sleep` child in the same process group, one per line;
//! `ECHO_CALLS` is a file each call's tool name (and `tools/list` for each listing) is
//! appended to; `ECHO_CONTROL=1` adds `ctl`, whose description and result hold control
//! characters; `ECHO_PAGES=endless` lists one tool per page with a next cursor, forever.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, SubscriptionFilter,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSink};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
struct State {
    extra: AtomicBool,
    sinks: Mutex<Vec<SubscriptionSink>>,
    peers: Mutex<Vec<Peer<RoleServer>>>,
}

#[derive(Clone, Default)]
struct Echo {
    state: Arc<State>,
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": schema})
}

fn object(props: Value) -> Value {
    json!({"type": "object", "properties": props})
}

fn tools(extra: bool) -> Vec<Value> {
    let mut v = vec![
        tool(
            "echo",
            "Echo the text back",
            object(json!({"text": {"type": "string"}})),
        ),
        tool("env", "Print the environment", object(json!({}))),
        tool(
            "slow",
            "Sleep, then answer",
            object(json!({"ms": {"type": "integer"}})),
        ),
        tool("mixed", "Text, an image and a link", object(json!({}))),
        tool("structured", "Only structured content", object(json!({}))),
        tool(
            "big",
            "A long answer",
            object(json!({"bytes": {"type": "integer"}})),
        ),
        tool("fail", "Always fails", object(json!({}))),
        tool("change_tools", "Add the `extra` tool", object(json!({}))),
    ];
    if extra {
        v.push(tool("extra", "Added later", object(json!({}))));
    }
    let filler: usize = std::env::var("ECHO_FILLER")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    for i in 0..filler {
        v.push(tool(&format!("filler_{i:03}"), "Filler", object(json!({}))));
    }
    if std::env::var_os("ECHO_BIG_SCHEMA").is_some() {
        let mut props = serde_json::Map::new();
        for i in 0..300 {
            props.insert(
                format!("field_{i:03}"),
                json!({"type": "string", "description": "a field"}),
            );
        }
        v.push(tool(
            "big_schema",
            "Huge schema",
            object(Value::Object(props)),
        ));
    }
    if std::env::var_os("ECHO_LONG_DESC").is_some() {
        v.push(tool("long_desc", &"d".repeat(3000), object(json!({}))));
    }
    if std::env::var_os("ECHO_CONTROL").is_some() {
        v.push(tool(
            "ctl",
            "red\u{1b}[31m text\u{7}\r\u{202e}reversed",
            object(json!({})),
        ));
    }
    for name in ["storm", "exit", "headers"] {
        v.push(tool(name, name, object(json!({}))));
    }
    v
}

/// Send `n` tool-list-changed notifications to every client that can receive them.
async fn notify(state: &State, n: usize) {
    let sinks: Vec<_> = state.sinks.lock().unwrap().clone();
    let peers: Vec<_> = state.peers.lock().unwrap().clone();
    for _ in 0..n {
        if sinks.is_empty() {
            for p in &peers {
                let _ = p.notify_tool_list_changed().await;
            }
        }
        for s in &sinks {
            let _ = s.notify_tool_list_changed().await;
        }
    }
}

fn log_call(name: &str) {
    if let Some(path) = std::env::var_os("ECHO_CALLS")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{name}");
    }
}

/// The result of calling `name`, as MCP JSON.
async fn call(name: &str, args: &Value, state: &State) -> Value {
    log_call(name);
    match name {
        "echo" => {
            json!({"content": [{"type": "text", "text": args["text"].as_str().unwrap_or("")}]})
        }
        "env" => {
            let mut lines: Vec<String> =
                std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
            lines.sort();
            json!({"content": [{"type": "text", "text": lines.join("\n")}]})
        }
        "slow" => {
            let ms = args["ms"].as_u64().unwrap_or(5000);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            json!({"content": [{"type": "text", "text": "done"}]})
        }
        "mixed" => json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
            {"type": "resource_link", "uri": "file:///tmp/report.txt", "name": "report"},
            {"type": "text", "text": "world"}
        ]}),
        "structured" => json!({"content": [], "structuredContent": {"answer": 42}}),
        "big" => {
            let n = args["bytes"].as_u64().unwrap_or(100_000) as usize;
            let text: String = (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            json!({"content": [{"type": "text", "text": text}]})
        }
        "fail" => json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
        "change_tools" => {
            state.extra.store(true, Ordering::SeqCst);
            notify(state, 1).await;
            json!({"content": [{"type": "text", "text": "changed"}]})
        }
        "storm" => {
            notify(state, 200).await;
            json!({"content": [{"type": "text", "text": "stormed"}]})
        }
        "exit" => {
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                std::process::exit(3);
            });
            json!({"content": [{"type": "text", "text": "bye"}]})
        }
        "ctl" => json!({"content": [{"type": "text", "text": "a\u{1b}[2Kb\u{7}c\td\ne"}]}),
        other => {
            json!({"content": [{"type": "text", "text": format!("no tool {other}")}], "isError": true})
        }
    }
}

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        log_call("tools/list");
        self.state.peers.lock().unwrap().push(context.peer.clone());
        let page = if std::env::var("ECHO_PAGES").as_deref() == Ok("endless") {
            let n: u64 = request
                .and_then(|r| r.cursor)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            json!({"tools": [tool(&format!("page_{n:06}"), "a page", object(json!({})))],
                   "nextCursor": (n + 1).to_string()})
        } else {
            let extra = self.state.extra.load(Ordering::SeqCst);
            json!({"tools": tools(extra)})
        };
        serde_json::from_value(page).map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let v = match context.extensions.get::<http::request::Parts>() {
            Some(parts) if request.name == "headers" => {
                let mut lines: Vec<String> = parts
                    .headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", String::from_utf8_lossy(v.as_bytes())))
                    .collect();
                lines.sort();
                json!({"content": [{"type": "text", "text": lines.join("\n")}]})
            }
            _ => call(&request.name, &args, &self.state).await,
        };
        let result: CallToolResult = serde_json::from_value(v)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(result.into())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        self.state
            .sinks
            .lock()
            .unwrap()
            .push(context.sink().clone());
        context.cancelled().await;
        Ok(())
    }
}

/// Write this process's pid and a `sleep` child's (same process group) to the pid file.
fn write_pids() {
    let Some(path) = std::env::var_os("ECHO_PID_FILE") else {
        return;
    };
    let child = std::process::Command::new("sleep")
        .arg("300")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let text = format!("{}\n{}\n", std::process::id(), child.id());
    std::fs::write(path, text).expect("write pid file");
    std::mem::forget(child);
}

async fn serve_stdio() {
    let running = Echo::default()
        .serve(rmcp::transport::stdio())
        .await
        .expect("serve stdio");
    let _ = running.waiting().await;
}

async fn serve_http(url_file: &str) {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    let echo = Echo::default();
    let service = StreamableHttpService::new(
        move || Ok(echo.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::fs::write(url_file, format!("http://{addr}/mcp")).expect("write url file");
    while let Ok((stream, _)) = listener.accept().await {
        let service = service.clone();
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let mut service = service.clone();
                    async move { tower_service::Service::call(&mut service, req).await }
                });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// A server from before `server/discover`: line-delimited JSON-RPC, `initialize` only.
async fn serve_legacy() {
    let state = State::default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // a notification
        };
        let method = msg["method"].as_str().unwrap_or("");
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "legacy-echo", "version": "0.1"}
            }}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => {
                let mut tools = vec![
                    tool(
                        "echo",
                        "Echo the text back",
                        object(json!({"text": {"type": "string"}})),
                    ),
                    tool("change_tools", "Add the `extra` tool", object(json!({}))),
                ];
                if state.extra.load(Ordering::SeqCst) {
                    tools.push(tool("extra", "Added later", object(json!({}))));
                }
                json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}})
            }
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("").to_string();
                let args = msg["params"]["arguments"].clone();
                let result = call(&name, &args, &state).await;
                if name == "change_tools" {
                    // Before 2026-07-28 a server sends notifications whenever it likes.
                    let note =
                        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
                    let mut bytes = serde_json::to_vec(&note).expect("json");
                    bytes.push(b'\n');
                    let _ = out.write_all(&bytes).await;
                }
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            _ => json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": format!("method not found: {method}")
            }}),
        };
        let mut bytes = serde_json::to_vec(&reply).expect("json");
        bytes.push(b'\n');
        if out.write_all(&bytes).await.is_err() || out.flush().await.is_err() {
            break;
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(ms) = std::env::var("ECHO_START_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    write_pids();
    match args.get(1).map(String::as_str) {
        Some("http") => serve_http(args.get(2).expect("URL_FILE")).await,
        Some("legacy") => serve_legacy().await,
        Some("hang") => std::future::pending::<()>().await,
        _ => serve_stdio().await,
    }
}
