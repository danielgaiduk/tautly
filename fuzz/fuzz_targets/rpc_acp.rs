//! JSON-RPC and ACP framing and request parsing. The input is a client's byte stream;
//! its first byte picks what reads it:
//! - 0: line framing (`LineReader`), `jsonrpc::parse` and `command_for` on each line;
//! - 1: the whole JSON-RPC server, whose engine factory always fails;
//! - 2: the whole ACP bridge, likewise;
//! - 3: the ACP mapping helpers on one JSON value (prompt blocks, MCP servers, tool
//!   arguments) and the unified-diff preview on the rest of the input.
//!
//! Nothing panics; framing yields the input's lines and nothing over the limit; a
//! command `command_for` accepts is never `Unknown`; every frame a server writes is a
//! JSON-RPC 2.0 object.
#![no_main]

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use tautly_acp::Bridge;
use tautly_acp::map;
use tautly_acp::schema::{
    ContentBlock, InitializeParams, McpServer, PermissionResponse, ToolCallContent,
};
use tautly_core::{Command, EngineError, Workspaces};
use tautly_rpc::jsonrpc::{self, Incoming, Line, LineReader};
use tautly_rpc::{Server, command_for};
use tokio::io::{AsyncWrite, BufReader};

const MAX: usize = 256;

/// Collects what a server writes.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl AsyncWrite for Sink {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        self.0.lock().unwrap().extend_from_slice(b);
        Poll::Ready(Ok(b.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn check_frames(out: &[u8]) {
    for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_slice(line).expect("a frame is JSON");
        assert_eq!(v["jsonrpc"], "2.0", "frame {v}");
    }
}

fn framing(data: &[u8]) {
    let rt = runtime();
    let lines = rt.block_on(async {
        // A tiny buffer, so lines arrive in pieces.
        let mut input = BufReader::with_capacity(7, data);
        let mut reader = LineReader::default();
        let mut lines = Vec::new();
        loop {
            match reader.read(&mut input, MAX).await.unwrap() {
                Line::Eof => break,
                l => lines.push(l),
            }
        }
        lines
    });
    let mut whole = Vec::new();
    let mut complete = true;
    for (i, l) in lines.iter().enumerate() {
        match l {
            Line::Line(l) => {
                assert!(l.len() <= MAX && !l.contains(&b'\n'));
                if i > 0 {
                    whole.push(b'\n');
                }
                whole.extend_from_slice(l);
                if let Ok(Incoming::Request { method, params, .. }) = jsonrpc::parse(l)
                    && let Ok(cmd) = command_for(&method, &params)
                {
                    assert!(!matches!(cmd, Command::Unknown), "{method} read as Unknown");
                }
            }
            Line::TooLong => complete = false,
            Line::Eof => unreachable!(),
        }
    }
    if complete {
        let trimmed = data.strip_suffix(b"\n").unwrap_or(data);
        assert_eq!(whole, trimmed, "framing lost or invented bytes");
    }
}

fn rpc_server(data: &[u8]) {
    let home = tempfile::tempdir().unwrap();
    let out = Sink::default();
    let factory: tautly_rpc::EngineFactory = Arc::new(|_| Err("no engines here".into()));
    runtime().block_on(async {
        let server = Server::new(factory, Workspaces::new(home.path()), home.path());
        let _ = server.serve(BufReader::new(data), out.clone()).await;
    });
    check_frames(&out.0.lock().unwrap());
}

fn acp_bridge(data: &[u8]) {
    let out = Sink::default();
    let factory: tautly_acp::AcpEngineFactory = Arc::new(|_, _, _| Err(EngineError::Closed));
    runtime().block_on(async {
        let _ = Bridge::new(factory).serve(data, out.clone()).await;
    });
    check_frames(&out.0.lock().unwrap());
}

fn helpers(data: &[u8]) {
    let (json, rest) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    if let Ok(v) = serde_json::from_slice::<Value>(json) {
        if let Ok(blocks) = serde_json::from_value::<Vec<ContentBlock>>(v.clone()) {
            let _ = map::prompt_text(&blocks);
        }
        if let Ok(servers) = serde_json::from_value::<Vec<McpServer>>(v.clone()) {
            let _ = map::client_mcp(servers);
        }
        let _ = serde_json::from_value::<InitializeParams>(v.clone()).map(|p| p.notices());
        let _ = serde_json::from_value::<PermissionResponse>(v.clone());
        for tool in ["bash", "edit", "read", "write", "grep", "mcp__s__t"] {
            let _ = map::tool_title(tool, &v);
            let _ = map::call_text(tool, &v);
            // Display only: an absolute path stays as the call named it.
            let _ = map::locations(root.path(), &v);
        }
    }
    let text = String::from_utf8_lossy(rest);
    let _ = map::slash(&text);
    let _ = map::single_line(&text);
    let _ = map::fenced(&text);
    let _ = map::approval_text(&text);
    for c in map::preview_diffs(root.path(), &text).unwrap_or_default() {
        if let ToolCallContent::Diff { path, .. } = c {
            assert!(
                path.starts_with(root.path()),
                "preview of {path:?} outside the root"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    match which % 4 {
        0 => framing(rest),
        1 => rpc_server(rest),
        2 => acp_bridge(rest),
        _ => helpers(rest),
    }
});
