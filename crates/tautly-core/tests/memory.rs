//! Project memory in the engine (spec 7.4, 7.5, 7.7): proposals and approvals, the memory
//! index in the stable prefix (changed only at cache epochs), episodes written by the side
//! model, consolidation, and pending proposals across a resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tautly_core::{
    BudgetLimits, Command, Config, Engine, EngineBuilder, Event, FinishReason, SessionRecord,
    UsageRole, Workspace,
};
use tautly_memory::{MemoryDraft, MemoryKind};
use tautly_provider::{
    Block, CanonicalRequest, Catalog, Content, MockProvider, MockTurn, ModelId, ProviderError,
    ProviderEvent, RawUsage, StopReason,
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
                ..
            } => Some((proposal_id.clone(), change.clone())),
            _ => None,
        })
        .collect()
}

/// The ids of the entries each proposal in `events` replaces, by proposal id.
fn replaced(events: &[Event]) -> Vec<(String, Vec<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::MemoryProposed {
                proposal_id,
                replaces,
                ..
            } => Some((
                proposal_id.clone(),
                replaces.iter().map(|r| r.id.clone()).collect(),
            )),
            _ => None,
        })
        .collect()
}

fn running(pattern: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    // A broken file's text (and the project's path) stays out of the warning.
    fx.write(".tautly/memory/broken.md", "---\n\"SECRET-CONTENT\n---\n");
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
    let root = fx.root().to_string_lossy().into_owned();
    assert!(
        !warnings(&events)
            .iter()
            .any(|w| w.contains("SECRET") || w.contains(&root)),
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
    // Nobody is asked, so nothing is shown as waiting for an answer.
    assert!(proposals(&events).is_empty(), "{events:?}");
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
    // Saved without asking: said, not shown as a proposal.
    assert!(proposals(&events).is_empty(), "{events:?}");
    assert!(
        warnings(&events)
            .iter()
            .any(|w| w.contains("approve_memory") && w.contains("Use pnpm")),
        "{events:?}"
    );
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
    run.send(Command::ConsolidateMemory {
        id: Some("m-1".into()),
    })
    .await;
    let mut events = run
        .until(|e| matches!(e, Event::MemoryProposed { .. }))
        .await;
    events.extend(
        run.until(|e| matches!(e, Event::MemoryProposed { .. }))
            .await,
    );
    let props = proposals(&events);
    assert_eq!(props.len(), 2);
    // Review CP5-A M5: the end, after every proposal, with the command's id.
    let end = run
        .until(|e| matches!(e, Event::ConsolidateFinished { .. }))
        .await;
    assert!(
        matches!(
            end.last(),
            Some(Event::ConsolidateFinished { id: Some(id), applied: 0, failed: 0 }) if id == "m-1"
        ),
        "{end:?}"
    );
    // Each shows the entries it replaces, so a client can show a diff.
    let mut shown: Vec<Vec<String>> = replaced(&events).into_iter().map(|(_, r)| r).collect();
    shown.sort();
    assert_eq!(shown, [vec!["a", "b"], vec!["c"]]);
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

#[tokio::test]
async fn an_update_shows_the_entry_it_replaces() {
    let fx = Fx::new();
    fx.entry_file("use-pnpm", "use-pnpm", "Use pnpm");
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call(
                "m1",
                "remember",
                json!({"id": "use-pnpm", "title": "Use pnpm only", "kind": "convention",
                       "body": "pnpm, never npm."}),
            ),
            remember("m2", "Brand new"),
        ]),
        text_step("noted"),
    ]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("update it").await;
    let shown: Vec<Vec<String>> = replaced(&events).into_iter().map(|(_, r)| r).collect();
    assert_eq!(shown, [vec!["use-pnpm".to_string()], vec![]], "{events:?}");
    let old = events
        .iter()
        .find_map(|e| match e {
            Event::MemoryProposed { replaces, .. } if !replaces.is_empty() => {
                Some(replaces[0].clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(old.title, "Use pnpm");
    assert_eq!(old.body, "Body of use-pnpm.");
}

#[tokio::test]
async fn exhausted_budget_sends_no_episode_or_consolidation_request() {
    let fx = Fx::new();
    fx.entry_file("a", "a", "Use pnpm");
    fx.entry_file("b", "b", "Always pnpm");
    let mut cfg = config(Mode::Ask);
    // The first response (110 tokens) uses it up.
    cfg.budget.session = BudgetLimits {
        max_usd: None,
        max_tokens: Some(50),
    };
    let mock = MockProvider::new(vec![
        text_step("done"),
        text_step("[]"),
        text_step("# Never asked"),
    ]);
    let mut run = start(fx.ready(cfg, &mock).await);
    run.turn_with("work").await;
    assert_eq!(mock.requests().len(), 1);
    run.send(Command::ConsolidateMemory { id: None }).await;
    let ev = run
        .until(|e| matches!(e, Event::Warning { message } if message.contains("consolidation")))
        .await;
    assert!(
        warnings(&ev)
            .iter()
            .any(|w| w.contains("consolidation") && w.contains("budget")),
        "{ev:?}"
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "consolidation asked the side model"
    );
    let session = run.engine.session_id().to_string();
    let after = run.stop().await;
    assert_eq!(mock.requests().len(), 1, "the episode asked the side model");
    assert!(
        warnings(&after)
            .iter()
            .any(|w| w.contains("episode") && w.contains("budget")),
        "{after:?}"
    );
    assert!(!fx.episode_path(&session).exists());
}

#[tokio::test]
async fn an_explicit_consolidation_is_held_back_only_by_the_session_budget() {
    let fx = Fx::new();
    fx.entry_file("a", "a", "Use pnpm");
    fx.entry_file("b", "b", "Always pnpm");
    let mut cfg = config(Mode::Ask);
    // The last turn used up its turn budget (110 of 50 tokens); the session has room.
    cfg.budget.turn = BudgetLimits {
        max_usd: None,
        max_tokens: Some(50),
    };
    let mock = MockProvider::new(vec![
        text_step("done"),
        text_step("[]"),
        text_step("# Never asked"),
    ]);
    let mut run = start(fx.ready(cfg, &mock).await);
    run.turn_with("work").await;
    run.send(Command::ConsolidateMemory { id: None }).await;
    let ev = run
        .until(|e| matches!(e, Event::Warning { message } if message.contains("consolidation")))
        .await;
    assert!(
        warnings(&ev).iter().any(|w| w.contains("no changes")),
        "{ev:?}"
    );
    assert_eq!(mock.requests().len(), 2, "consolidation was not asked for");
    // The episode, which nobody asked for, still respects every budget.
    let session = run.engine.session_id().to_string();
    let after = run.stop().await;
    assert_eq!(mock.requests().len(), 2, "the episode asked the side model");
    assert!(
        warnings(&after)
            .iter()
            .any(|w| w.contains("episode") && w.contains("budget")),
        "{after:?}"
    );
    assert!(!fx.episode_path(&session).exists());
}

#[tokio::test]
async fn consolidation_prompt_fits_the_side_model_window() {
    let fx = Fx::new();
    for i in 0..40 {
        fx.write(
            &format!(".tautly/memory/e{i:02}.md"),
            &format!(
                "---\nid: e{i:02}\ntitle: \"Entry {i:02}\"\nkind: fact\ncreated: 2026-01-01T00:00:00\n---\n{}\n",
                "a fact worth keeping ".repeat(50)
            ),
        );
    }
    let mut cfg = config(Mode::Ask);
    // 6,000 tokens: the side runner takes about 10,500 bytes of input.
    cfg.catalog_overrides = Some(catalog_with(6_000, 1_000, "none"));
    let mock = MockProvider::new(vec![text_step("[]")]);
    let mut run = start(fx.ready(cfg, &mock).await);
    run.send(Command::ConsolidateMemory { id: None }).await;
    run.until(|e| matches!(e, Event::Warning { message } if message.contains("no changes")))
        .await;
    let text = request_text(&mock.requests()[0]);
    assert!(text.len() <= 12_000, "{} bytes", text.len());
    assert!(text.contains("left out"), "{text}");
}

#[tokio::test]
async fn shutdown_stops_background_commands_before_the_episode() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        // A background child that outlives its call (a dev server, say).
        tools_step(vec![call(
            "c1",
            "bash",
            json!({"command": "sleep 317 >/dev/null 2>&1 &"}),
        )]),
        text_step("started"),
        // The session-end episode, which takes its time.
        MockTurn::Stall,
    ]);
    let mut run = start(fx.ready(config(Mode::Yolo), &mock).await);
    run.turn_with("start the server").await;
    assert!(running("^sleep 317$"), "the background child should run");
    run.send(Command::Shutdown).await;
    let deadline = Instant::now() + WAIT;
    while mock.requests().len() < 3 {
        assert!(Instant::now() < deadline, "no episode request");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The episode is being written; background commands were stopped before it.
    assert!(
        !running("^sleep 317$"),
        "the background child outlived the session into its episode"
    );
    let closed =
        tokio::time::timeout(WAIT, async { while run.events.recv().await.is_some() {} }).await;
    assert!(closed.is_ok(), "the engine did not stop");
}

#[tokio::test]
async fn no_episode_for_a_session_without_a_reply() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        MockTurn::Error(ProviderError::Auth("no key".into())),
        text_step("# Never asked"),
    ]);
    let mut run = start(fx.ready(config(Mode::Ask), &mock).await);
    let events = run.turn_with("hello").await;
    assert!(
        matches!(
            events.last(),
            Some(Event::TurnFinished {
                reason: FinishReason::Error,
                ..
            })
        ),
        "{events:?}"
    );
    let session = run.engine.session_id().to_string();
    run.stop().await;
    assert_eq!(
        mock.requests().len(),
        1,
        "an episode of nothing was asked for"
    );
    assert!(!fx.episode_path(&session).exists());
}
