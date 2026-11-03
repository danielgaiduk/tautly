//! Project memory in the engine (spec 7.4, 7.5, 7.7): proposals and approvals, the memory
//! index in the stable prefix (changed only at cache epochs), episodes written by the side
//! model, consolidation, and pending proposals across a resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_core::{
    Command, Config, Engine, EngineBuilder, Event, FinishReason, SessionRecord, UsageRole,
    Workspace,
};
use tautly_memory::{MemoryDraft, MemoryKind};
use tautly_provider::{
    Block, CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, ProviderEvent,
    RawUsage, StopReason,
};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_session::SessionReader;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);
const OVER_BUDGET: &str = "memory index over budget — run /memory consolidate";

fn catalog_with(window: u32, max_output: u32, cache: &str) -> Catalog {
    Catalog::from_toml(&format!(
        r#"[models."{MODEL}"]
context_window = {window}
max_output = {max_output}
parallel_tools = true
reasoning = "none"
cache = "{cache}"
images = false
edit_format = "str_replace"
"#
    ))
    .unwrap()
}

fn config(mode: Mode) -> Config {
    let mut c = Config {
        model: ModelId::from(MODEL),
        catalog_overrides: Some(catalog_with(200_000, 8_000, "none")),
        ..Config::default()
    };
    c.permissions.mode = mode;
    c
}

fn call(id: &str, name: &str, args: Value) -> ProviderEvent {
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

fn tools_step(calls: Vec<ProviderEvent>) -> MockTurn {
    let mut evs = calls;
    evs.push(ProviderEvent::Done {
        stop: StopReason::ToolUse,
    });
    MockTurn::Events(evs)
}

fn text_step(text: &str) -> MockTurn {
    MockTurn::Events(vec![
        ProviderEvent::TextDelta(text.into()),
        usage(),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
}

fn remember(id: &str, title: &str) -> ProviderEvent {
    call(
        id,
        "remember",
        json!({"title": title, "kind": "convention", "body": "Run pnpm install, never npm.",
               "tags": ["build"]}),
    )
}

struct Fx {
    dir: TempDir,
    home: TempDir,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            dir: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn memory_dir(&self) -> PathBuf {
        self.dir.path().join(".tautly/memory")
    }

    /// The `.md` files of project memory with their text, by name.
    fn memory_files(&self) -> Vec<(String, String)> {
        let Ok(read) = std::fs::read_dir(self.memory_dir()) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = read
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(&p).unwrap(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn entry_file(&self, name: &str, id: &str, title: &str) {
        self.write(
            &format!(".tautly/memory/{name}.md"),
            &format!(
                "---\nid: {id}\ntitle: \"{title}\"\nkind: fact\ncreated: 2026-01-01T00:00:00\n---\nBody of {id}.\n"
            ),
        );
    }

    fn episode_path(&self, session_id: &str) -> PathBuf {
        self.home
            .path()
            .join("memory")
            .join(tautly_session::paths::project_id(&self.root()))
            .join("episodes")
            .join(format!("{session_id}.md"))
    }

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            interactive: true,
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    async fn ready_workspace(&self) -> Arc<Workspace> {
        let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(self.root()).unwrap());
        let ws = Workspace::new(sb, self.home.path());
        ws.start();
        assert!(ws.wait_ready(WAIT).await, "index never became ready");
        // The memory store opens right after the index.
        for _ in 0..300 {
            if ws.memory().is_some() {
                return ws;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("memory never opened");
    }

    /// `ready_workspace` with the builder using it.
    async fn ready(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            workspace: Some(self.ready_workspace().await),
            ..self.builder(cfg, mock)
        }
    }
}

struct Run {
    engine: Engine,
    events: mpsc::Receiver<Event>,
}

fn start(b: EngineBuilder) -> Run {
    let (engine, events) = b.build().unwrap();
    Run { engine, events }
}

impl Run {
    async fn send(&self, cmd: Command) {
        self.engine.send(cmd).await.unwrap();
    }

    async fn next(&mut self) -> Event {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed")
    }

    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let ev = self.next().await;
            let done = pred(&ev);
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    async fn turn_with(&mut self, text: &str) -> Vec<Event> {
        self.send(Command::SendMessage { text: text.into() }).await;
        self.until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
    }

    /// Shut down; the events until the channel closes.
    async fn stop(mut self) -> Vec<Event> {
        self.send(Command::Shutdown).await;
        let mut out = Vec::new();
        let closed = tokio::time::timeout(WAIT, async {
            while let Some(ev) = self.events.recv().await {
                out.push(ev);
            }
        })
        .await;
        assert!(closed.is_ok(), "the engine did not stop");
        out
    }

    fn records(&self) -> Vec<SessionRecord> {
        records_at(&self.engine.current_session().1)
    }

    /// Wait until the log holds a record matching `pred` (commands run in the actor).
    async fn logged(&self, pred: impl Fn(&SessionRecord) -> bool) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.records().iter().any(&pred) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never logged: {:#?}", self.records());
    }
}

fn records_at(path: &Path) -> Vec<SessionRecord> {
    let reader = SessionReader::<SessionRecord>::open(path).unwrap();
    reader.records().map(|r| r.unwrap().1).collect()
}

fn system_block(req: &CanonicalRequest, i: usize) -> String {
    match &req.system[i] {
        Block::Text(t) => t.clone(),
        other => panic!("not text: {other:?}"),
    }
}

/// The memory index block of a request (the stable prefix's third block).
fn memory_index(req: &CanonicalRequest) -> String {
    system_block(req, 2)
}

fn request_text(req: &CanonicalRequest) -> String {
    let mut out = String::new();
    for b in &req.system {
        if let Block::Text(t) = b {
            out.push_str(t);
        }
    }
    for m in &req.messages {
        for c in &m.content {
            match c {
                Content::Text(t) => out.push_str(t),
                Content::ToolResult { output, .. } => out.push_str(output),
                _ => {}
            }
        }
    }
    out
}

fn proposals(events: &[Event]) -> Vec<(String, tautly_core::MemoryChange)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::MemoryProposed {
                proposal_id,
                change,
            } => Some((proposal_id.clone(), change.clone())),
            _ => None,
        })
        .collect()
}

fn tool_output(events: &[Event], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            Event::ToolFinished {
                call_id,
                output,
                is_error,
                ..
            } if call_id == id => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}: {events:?}"))
}

fn warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Warning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn memory_index_rendered_after_agents_md_in_stable_block() {
    let fx = Fx::new();
    fx.write("AGENTS.md", "AGENTS-RULE: keep it short.\n");
    fx.write("src/lib.rs", "pub fn alpha() {}\n");
    fx.entry_file("use-pnpm", "use-pnpm", "Use pnpm");
    fx.write(".tautly/memory/broken.md", "not an entry");
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("hi").await;
    // What could not be loaded is said at session start.
    assert!(
        warnings(&events)
            .iter()
            .any(|w| w.contains("1 memory entry could not be loaded") && w.contains("broken.md")),
        "{events:?}"
    );
    let req = &mock.requests()[0];
    let agents = system_block(req, 1);
    let memory = memory_index(req);
    assert!(
        agents.contains("<agents_md>") && agents.contains("AGENTS-RULE"),
        "{agents}"
    );
    assert!(memory.contains("<memory_index>"), "{memory}");
    assert!(memory.contains("- [fact] Use pnpm"), "{memory}");
    // Both are in the cached (stable) system prefix, the index right after AGENTS.md.
    let texts: Vec<String> = (0..req.system.len())
        .map(|i| system_block(req, i))
        .collect();
    let at = |needle: &str| texts.iter().position(|t| t.contains(needle)).unwrap();
    assert_eq!(at("AGENTS-RULE") + 1, at("- [fact] Use pnpm"));
}

#[tokio::test]
async fn remember_emits_proposal_and_writes_file_only_after_approve() {
    let fx = Fx::new();
    fx.write("lib.rs", "fn a(){1}\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![
            remember("m1", "Use pnpm"),
            // An error result is never proposed.
            call(
                "m2",
                "remember",
                json!({"title": "Bad", "kind": "fact", "body": "b",
                       "anchors": [{"path": "missing.rs"}]}),
            ),
        ]),
        text_step("noted"),
        text_step("ok"),
    ]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("remember pnpm").await;
    let props = proposals(&events);
    assert_eq!(props.len(), 1, "{events:?}");
    let (id, change) = &props[0];
    let entry = change.upsert.as_ref().unwrap();
    assert_eq!(entry.title, "Use pnpm");
    assert_eq!(entry.tags, ["build"]);
    // Shown after the call's result, which says it waits for the user.
    let finished_at = events
        .iter()
        .position(|e| matches!(e, Event::ToolFinished { call_id, .. } if call_id == "m1"))
        .unwrap();
    let proposed_at = events
        .iter()
        .position(|e| matches!(e, Event::MemoryProposed { .. }))
        .unwrap();
    assert!(finished_at < proposed_at);
    let (out, is_error) = tool_output(&events, "m1");
    assert!(!is_error && out.contains("awaiting"), "{out}");
    assert!(tool_output(&events, "m2").1);
    // Nothing written yet; the proposal is in the log.
    assert!(fx.memory_files().is_empty());
    assert!(
        run.records()
            .iter()
            .any(|r| matches!(r, SessionRecord::MemoryProposal { id: p, .. } if p == id))
    );

    run.send(Command::ApproveMemory {
        proposal_id: id.clone(),
    })
    .await;
    run.logged(|r| matches!(r, SessionRecord::MemoryDecision { id: p, approved: true } if p == id))
        .await;
    let files = fx.memory_files();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].0, "use-pnpm.md");
    assert!(files[0].1.contains("title: \"Use pnpm\"") && files[0].1.contains("  - build"));
    // The next turn (cold cache: this model has none) carries it in the index.
    run.turn_with("next").await;
    let reqs = mock.requests();
    assert!(!memory_index(&reqs[0]).contains("Use pnpm"));
    assert!(memory_index(&reqs[2]).contains("- [convention] Use pnpm"));
    // A decided proposal cannot be decided again.
    run.send(Command::ApproveMemory {
        proposal_id: id.clone(),
    })
    .await;
    let ev = run.until(|e| matches!(e, Event::Warning { .. })).await;
    assert!(warnings(&ev)[0].contains("no memory proposal"), "{ev:?}");
}

#[tokio::test]
async fn reject_writes_nothing() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![remember("m1", "Use pnpm")]),
        text_step("noted"),
        text_step("ok"),
    ]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("remember").await;
    let (id, _) = proposals(&events).pop().unwrap();
    run.send(Command::RejectMemory {
        proposal_id: id.clone(),
    })
    .await;
    run.logged(
        |r| matches!(r, SessionRecord::MemoryDecision { id: p, approved: false } if p == &id),
    )
    .await;
    run.turn_with("next").await;
    assert!(fx.memory_files().is_empty());
    assert!(!memory_index(&mock.requests()[2]).contains("Use pnpm"));

    // Headless: nobody can approve, so it is rejected at once (and said).
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![remember("m1", "Use pnpm")]),
        text_step("noted"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: false,
        ..fx.ready(config(Mode::Ask), &mock).await
    });
    let events = run.turn_with("remember").await;
    let (out, _) = tool_output(&events, "m1");
    assert!(out.contains("not saved"), "{out}");
    assert!(
        warnings(&events)
            .iter()
            .any(|w| w.contains("--approve-memory")),
        "{events:?}"
    );
    assert!(fx.memory_files().is_empty());
    assert!(run.records().iter().any(|r| matches!(
        r,
        SessionRecord::MemoryDecision {
            approved: false,
            ..
        }
    )));

    // Headless with `approve_memory`: written at once.
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![remember("m1", "Use pnpm")]),
        text_step("noted"),
    ]);
    let mut cfg = config(Mode::Ask);
    cfg.approve_memory = true;
    let mut run = start(EngineBuilder {
        interactive: false,
        ..fx.ready(cfg, &mock).await
    });
    let events = run.turn_with("remember").await;
    let (out, _) = tool_output(&events, "m1");
    assert!(out.contains("saved"), "{out}");
    assert_eq!(fx.memory_files().len(), 1);
}

#[tokio::test]
async fn stale_flag_appears_at_next_cache_epoch() {
    let fx = Fx::new();
    fx.write("src/lib.rs", "pub fn alpha() -> u8 {\n    1\n}\n");
    let ws = fx.ready_workspace().await;
    {
        let memory = ws.memory().unwrap();
        let mut store = memory.write().unwrap();
        let draft = MemoryDraft {
            id: None,
            title: "About alpha".into(),
            kind: MemoryKind::Fact,
            tags: vec![],
            anchors: vec![(PathBuf::from("src/lib.rs"), Some("alpha".into()))],
            body: "alpha returns one.".into(),
        };
        let change = store
            .draft_to_change(draft, &|s| s.len().div_ceil(4) as u32)
            .unwrap();
        store.apply(&change).unwrap();
    }
    // A cache that stays warm for 2 s.
    let mut cfg = config(Mode::AutoEdit);
    let cat = Catalog::from_toml(&format!(
        "[models.\"{MODEL}\"]\ncontext_window = 200000\nmax_output = 8000\n\
         parallel_tools = true\nreasoning = \"none\"\ncache = \"explicit\"\n\
         cache_ttl_s = 2\nimages = false\nedit_format = \"str_replace\"\n"
    ))
    .unwrap();
    cfg.catalog_overrides = Some(cat);
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "src/lib.rs"}))]),
        tools_step(vec![call(
            "e1",
            "edit",
            json!({"path": "src/lib.rs", "old_text": "    1\n", "new_text": "    2\n"}),
        )]),
        text_step("done"),
        text_step("ok"),
    ]);
    let mut run = start(EngineBuilder {
        workspace: Some(ws),
        ..fx.builder(cfg, &mock)
    });
    let events = run.turn_with("change alpha").await;
    assert_eq!(
        events.last(),
        Some(&Event::TurnFinished {
            reason: FinishReason::Completed,
            evidence: None
        })
    );
    assert!(!tool_output(&events, "e1").1, "{events:?}");
    let reqs = mock.requests();
    assert!(memory_index(&reqs[0]).contains("- [fact] About alpha"));
    // The anchored symbol changed at step 2, but the cache is warm: the prefix holds.
    for r in &reqs[..3] {
        assert!(!memory_index(r).contains("[stale]"), "{}", memory_index(r));
        assert_eq!(r.system, reqs[0].system);
    }
    // Once the cache has gone cold, the next turn shows the flag.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    run.turn_with("again").await;
    let reqs = mock.requests();
    assert!(
        memory_index(&reqs[3]).contains("- [fact] About alpha [stale]"),
        "{}",
        memory_index(&reqs[3])
    );
    assert!(run.records().iter().any(
        |r| matches!(r, SessionRecord::MemoryIndexApplied { text } if text.contains("[stale]"))
    ));
}

#[tokio::test]
async fn episode_written_by_side_model_at_shutdown() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        text_step("fixed it"),
        text_step("# Fix the flaky test\nOutcome: fixed by seeding the rng."),
    ]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    run.turn_with("fix the flaky test").await;
    let session = run.engine.session_id().to_string();
    let after = run.stop().await;
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    // Asked of the side model (here the main one: no small model), bare, with the session.
    let ep = &reqs[1];
    assert!(ep.tools.is_empty());
    assert!(request_text(ep).contains("fix the flaky test"), "{ep:?}");
    assert!(request_text(ep).to_lowercase().contains("what failed"));
    assert!(after.iter().any(|e| matches!(
        e,
        Event::Usage {
            role: UsageRole::Side,
            ..
        }
    )));
    let text = std::fs::read_to_string(fx.episode_path(&session)).unwrap();
    assert!(text.contains("seeding the rng"), "{text}");
}

#[tokio::test]
async fn episode_written_after_compaction() {
    let fx = Fx::new();
    for i in 1..=4 {
        fx.write(&format!("f{i}.txt"), &format!("file {i}\n"));
    }
    let mut cfg = config(Mode::Ask);
    cfg.catalog_overrides = Some(catalog_with(16_000, 1_000, "none"));
    let mut script: Vec<MockTurn> = (1..=4)
        .map(|i| {
            tools_step(vec![call(
                &format!("r{i}"),
                "read",
                json!({"path": format!("f{i}.txt")}),
            )])
        })
        .collect();
    script.push(text_step("SUMMARY-TEXT"));
    script.push(text_step("done"));
    script.push(text_step("# Read four files\nEPISODE-TEXT"));
    let mock = MockProvider::new(script);
    let mut run = start(fx.ready(cfg, &mock).await);
    let prompt = format!(
        "PROMPT-MARKER {}",
        "lorem ipsum dolor sit amet ".repeat(1500)
    );
    let events = run.turn_with(&prompt).await;
    assert_eq!(
        events.last(),
        Some(&Event::TurnFinished {
            reason: FinishReason::Completed,
            evidence: None
        }),
        "{events:?}"
    );
    assert!(events.iter().any(|e| matches!(e, Event::Compacted { .. })));
    // At the compaction the episode is the side model's summary of what was compacted
    // (goal, failed attempts, files, progress), under the session's task: no extra request.
    let session = run.engine.session_id().to_string();
    let text = std::fs::read_to_string(fx.episode_path(&session)).unwrap();
    let heading = text.lines().next().unwrap();
    assert!(heading.starts_with("# PROMPT-MARKER lorem"), "{text}");
    assert!(heading.chars().count() < 140, "{heading}");
    assert!(text.contains("SUMMARY-TEXT"), "{text}");
    assert_eq!(mock.requests().len(), 6);
    // At the end the side model writes the whole session's, replacing it.
    run.stop().await;
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 7);
    assert!(
        request_text(&reqs[6])
            .to_lowercase()
            .contains("what failed")
    );
    assert!(
        request_text(&reqs[6]).contains("SUMMARY-TEXT"),
        "the episode sees the summary"
    );
    let text = std::fs::read_to_string(fx.episode_path(&session)).unwrap();
    assert!(
        text.contains("EPISODE-TEXT") && !text.contains("SUMMARY-TEXT"),
        "{text}"
    );
}

#[tokio::test]
async fn over_budget_index_adds_warning() {
    let fx = Fx::new();
    for i in 0..60 {
        fx.entry_file(
            &format!("e{i:02}"),
            &format!("e{i:02}"),
            &format!("entry number {i:02} with a title of moderate length padding"),
        );
    }
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("hi").await;
    let status = events
        .iter()
        .find_map(|e| match e {
            Event::ContextStatus { warnings, .. } => Some(warnings.clone()),
            _ => None,
        })
        .expect("no ContextStatus");
    assert!(status.iter().any(|w| w == OVER_BUDGET), "{status:?}");
    // Few entries: no warning.
    let fx = Fx::new();
    fx.entry_file("a", "a", "A");
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("hi").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ContextStatus { warnings, .. }
        if warnings.iter().any(|w| w == OVER_BUDGET)))
    );
}

#[tokio::test]
async fn consolidate_emits_proposals_and_applies_only_approved() {
    let fx = Fx::new();
    fx.entry_file("a", "a", "Use pnpm");
    fx.entry_file("b", "b", "Always pnpm");
    fx.entry_file("c", "c", "Old note");
    let reply = json!([
        {"merge": ["a", "b"], "into": {"title": "Use pnpm only", "kind": "convention",
                                     "body": "pnpm, never npm."}},
        {"delete": "c"}
    ]);
    let mock = MockProvider::new(vec![text_step(&reply.to_string())]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    run.send(Command::ConsolidateMemory).await;
    let mut events = run
        .until(|e| matches!(e, Event::MemoryProposed { .. }))
        .await;
    events.extend(
        run.until(|e| matches!(e, Event::MemoryProposed { .. }))
            .await,
    );
    let props = proposals(&events);
    assert_eq!(props.len(), 2);
    let req = &mock.requests()[0];
    assert!(request_text(req).contains("Old note"), "{req:?}");
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Usage {
            role: UsageRole::Side,
            ..
        }
    )));
    // Nothing changes before a decision.
    assert_eq!(fx.memory_files().len(), 3);
    let (merge, _) = props
        .iter()
        .find(|(_, c)| c.upsert.is_some())
        .unwrap()
        .clone();
    let (delete, _) = props
        .iter()
        .find(|(_, c)| c.upsert.is_none())
        .unwrap()
        .clone();
    run.send(Command::ApproveMemory {
        proposal_id: merge.clone(),
    })
    .await;
    run.send(Command::RejectMemory {
        proposal_id: delete.clone(),
    })
    .await;
    run.logged(
        |r| matches!(r, SessionRecord::MemoryDecision { id, approved: false } if id == &delete),
    )
    .await;
    let names: Vec<String> = fx.memory_files().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["c.md", "use-pnpm-only.md"]);
}

#[tokio::test]
async fn pending_proposal_survives_resume() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![remember("m1", "Use pnpm")]),
        text_step("noted"),
    ]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("remember").await;
    let (id, _) = proposals(&events).pop().unwrap();
    let session = run.engine.session_id().to_string();
    run.stop().await;
    assert!(fx.memory_files().is_empty());

    let mock = MockProvider::new(vec![]);
    let mut run = start(EngineBuilder {
        resume: Some(session),
        ..fx.ready(config(Mode::Ask), &mock).await
    });
    // The client of the resumed session is told what is still waiting.
    let shown = run
        .until(|e| matches!(e, Event::MemoryProposed { .. }))
        .await;
    assert_eq!(proposals(&shown)[0].0, id);
    run.send(Command::ApproveMemory {
        proposal_id: id.clone(),
    })
    .await;
    run.logged(
        |r| matches!(r, SessionRecord::MemoryDecision { id: p, approved: true } if p == &id),
    )
    .await;
    assert_eq!(fx.memory_files().len(), 1);
}
