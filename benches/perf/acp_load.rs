//! ACP `session/load` of a large log keeps the agent's memory bounded (CP5-A M7: the
//! history streams to the editor instead of being collected first). The log is 200 MB by
//! default (`TAUTLY_PERF_LOG_MB`), made by a session whose long answers compact again and
//! again; the load runs in a fresh process, so its RSS is the load's alone.
//!
//! `cargo test --release -p tautly-perf --test acp_load -- --ignored`

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt as _;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tautly_acp::{Bridge, shared_factory};
use tautly_core::{Command, Config, EngineBuilder, Event, Workspaces};
use tautly_perf::{MB, peak_rss, report, rss};
use tautly_provider::{
    CanonicalRequest, Catalog, ModelId, Provider, ProviderError, ProviderEvent, StopReason,
};
use tautly_sandbox::Mode;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "mock/test";
const PROBE: &str = "TAUTLY_PERF_LOAD_PROBE";
/// Each answer's size: about 64k tokens, so the 200k window compacts every few turns.
const ANSWER: usize = 256 * 1024;

/// A long answer to every request with tools; a short summary to one without.
#[derive(Default)]
struct Long {
    n: AtomicUsize,
}

impl Provider for Long {
    fn name(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        req: CanonicalRequest,
        _cancel: CancellationToken,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let text = if req.tools.is_empty() {
            "Summary: long answers so far.".to_string()
        } else {
            let n = self.n.fetch_add(1, Ordering::Relaxed);
            format!("answer {n}: {}\n", "lorem ipsum ".repeat(ANSWER / 12))
        };
        let evs = vec![
            ProviderEvent::TextDelta(text),
            ProviderEvent::Done {
                stop: StopReason::EndTurn,
            },
        ];
        futures::stream::iter(evs.into_iter().map(Ok)).boxed()
    }
}

fn config() -> Config {
    let catalog = Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = 200000
max_output = 8000
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
"#
    ))
    .unwrap();
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog),
        ..Config::default()
    };
    c.permissions.mode = Mode::Yolo;
    c.checkpoints.enabled = false;
    c
}

fn builder(root: &Path, home: &Path) -> EngineBuilder {
    let provider = Arc::new(Long::default());
    EngineBuilder {
        provider: Some(provider.clone()),
        side_provider: Some(provider),
        data_home: Some(home.to_path_buf()),
        ..EngineBuilder::new(config(), root)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf gate"]
async fn session_load_of_a_large_log_keeps_rss_bounded() {
    let log_mb: u64 = std::env::var("TAUTLY_PERF_LOG_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let (dir, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = dir.path().canonicalize().unwrap();
    let (engine, mut events) = builder(&root, home.path()).build().unwrap();
    let id = engine.session_id().to_string();
    let log = engine.session_path().to_path_buf();
    let mut turns = 0;
    while std::fs::metadata(&log).map_or(0, |m| m.len()) < log_mb * MB {
        turns += 1;
        engine
            .send(Command::SendMessage {
                id: None,
                text: format!("question {turns}"),
            })
            .await
            .unwrap();
        while !matches!(events.recv().await.unwrap(), Event::TurnFinished { .. }) {}
    }
    engine.send(Command::Shutdown).await.unwrap();
    while events.recv().await.is_some() {}
    let size = std::fs::metadata(&log).unwrap().len();

    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["load_probe", "--exact", "--ignored", "--nocapture"])
        .env(
            PROBE,
            format!("{}\n{}\n{id}", root.display(), home.path().display()),
        )
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let field = |name: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {name} in {text}"))
    };
    let (grown, updates) = (field("load grew RSS by "), field("updates "));
    report(&format!(
        "acp: session/load of a {} MB log ({turns} turns), {updates} updates, RSS grew by {:.1} MB",
        size / MB,
        grown as f64 / MB as f64
    ));
    assert!(
        updates as usize >= turns,
        "{updates} updates for {turns} turns"
    );
    // Streaming holds a bounded window of updates, not the history.
    assert!(grown < 64 * MB, "{grown}");
}

/// Run as a child process by the gate: loads the session it names over ACP and prints
/// how much the load grew this process's peak RSS.
#[test]
#[ignore = "perf gate helper"]
fn load_probe() {
    let Ok(spec) = std::env::var(PROBE) else {
        return;
    };
    let mut parts = spec.lines();
    let (root, home, id) = (
        parts.next().unwrap().to_string(),
        parts.next().unwrap().to_string(),
        parts.next().unwrap().to_string(),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let data = std::path::PathBuf::from(&home);
        let factory = shared_factory(Workspaces::new(&data), move |root, resume, mcp, ws| {
            EngineBuilder {
                workspace: Some(ws),
                interactive: true,
                resume,
                client_mcp: mcp,
                ..builder(&root, &data)
            }
            .build()
        });
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (their_read, their_write) = tokio::io::split(theirs);
        let serve = tokio::spawn(Bridge::new(factory).serve(their_read, their_write));
        let (read, mut write) = tokio::io::split(ours);
        let mut lines = BufReader::new(read).lines();
        async fn send(w: &mut (impl AsyncWriteExt + Unpin), v: Value) {
            w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        }
        send(
            &mut write,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": 1, "clientCapabilities": {}}}),
        )
        .await;
        while let Some(line) = lines.next_line().await.unwrap() {
            if serde_json::from_str::<Value>(&line).unwrap()["id"] == 1 {
                break;
            }
        }
        let before = rss();
        send(
            &mut write,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/load",
                    "params": {"sessionId": id, "cwd": root, "mcpServers": []}}),
        )
        .await;
        let mut updates = 0u64;
        while let Some(line) = lines.next_line().await.unwrap() {
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 2 {
                assert!(v.get("error").is_none(), "{v}");
                break;
            }
            if v["method"] == "session/update" {
                updates += 1;
            }
        }
        println!("load grew RSS by {}", peak_rss().saturating_sub(before));
        println!("updates {updates}");
        drop(lines);
        serve.abort();
    });
}
