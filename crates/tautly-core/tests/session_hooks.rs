//! What the engine writes into the session log for time travel (spec 6.5 H2, H3, H5-H8):
//! prefix inputs, richer request records, post-images, approval requests, root-relative
//! paths, the header's config hash, and labels.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tautly_core::record::{ApprovalKind, SessionRecord, session_label};
use tautly_core::{Command, Config, Engine, EngineBuilder, Event, FinishReason, Workspace};
use tautly_provider::{
    Block, CanonicalRequest, Catalog, MockProvider, MockTurn, ModelId, ProviderEvent, RawUsage,
    StopReason,
};
use tautly_sandbox::{LocalSandbox, Mode, Sandbox};
use tautly_session::{BlobRef, HeaderInit, SessionReader, SessionWriter, paths};
use tautly_tools::PersistedEffect;
use tempfile::TempDir;
use tokio::sync::mpsc;

const MODEL: &str = "mock/test";
const WAIT: Duration = Duration::from_secs(30);

fn config(mode: Mode) -> Config {
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
        ProviderEvent::Usage(RawUsage {
            input: 100,
            output: 10,
            cache_read: 0,
            cache_write: 0,
        }),
        ProviderEvent::Done {
            stop: StopReason::EndTurn,
        },
    ])
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

    fn builder(&self, cfg: Config, mock: &MockProvider) -> EngineBuilder {
        EngineBuilder {
            provider: Some(Arc::new(mock.clone())),
            data_home: Some(self.home.path().to_path_buf()),
            ..EngineBuilder::new(cfg, self.dir.path())
        }
    }

    async fn ready_workspace(&self) -> Arc<Workspace> {
        let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::new(self.root()).unwrap());
        let ws = Workspace::new(sb, self.home.path());
        ws.start();
        assert!(ws.wait_ready(WAIT).await, "index never became ready");
        ws
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
    async fn say(&self, text: &str) {
        self.engine
            .send(Command::SendMessage { text: text.into() })
            .await
            .unwrap();
    }

    async fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("timed out waiting for an event")
                .expect("event channel closed");
            let done = pred(&ev);
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    async fn turn_with(&mut self, text: &str) -> FinishReason {
        self.say(text).await;
        match self
            .until(|e| matches!(e, Event::TurnFinished { .. }))
            .await
            .last()
        {
            Some(Event::TurnFinished { reason }) => *reason,
            _ => unreachable!(),
        }
    }

    fn reader(&self) -> SessionReader<SessionRecord> {
        SessionReader::open(self.engine.session_path()).unwrap()
    }

    fn records(&self) -> Vec<SessionRecord> {
        self.reader().records().map(|r| r.unwrap().1).collect()
    }
}

fn hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn text_of(req: &CanonicalRequest, i: usize) -> String {
    match &req.system[i] {
        Block::Text(t) => t.clone(),
        other => panic!("not text: {other:?}"),
    }
}

fn blob_text(reader: &SessionReader<SessionRecord>, b: &BlobRef) -> String {
    String::from_utf8(reader.read_blob(b).unwrap()).unwrap()
}

fn prefixes(recs: &[SessionRecord]) -> Vec<(usize, &SessionRecord)> {
    recs.iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, SessionRecord::PrefixApplied { .. }))
        .collect()
}

fn request_positions(recs: &[SessionRecord]) -> Vec<usize> {
    recs.iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, SessionRecord::Request { .. }))
        .map(|(i, _)| i)
        .collect()
}

#[tokio::test]
async fn prefix_applied_logged_before_first_request_with_agents_md_and_repo_map_blobs() {
    let fx = Fx::new();
    fx.write("AGENTS.md", "# Rules\nUse pnpm, never npm.\n");
    fx.write(
        "src/lib.rs",
        "pub fn alpha() {}\npub fn beta() { alpha() }\n",
    );
    let ws = fx.ready_workspace().await;
    let mock = MockProvider::new(vec![text_step("ok")]);
    let mut run = start(EngineBuilder {
        workspace: Some(ws),
        ..fx.builder(config(Mode::Ask), &mock)
    });
    assert_eq!(run.turn_with("hi").await, FinishReason::Completed);

    let recs = run.records();
    let found = prefixes(&recs);
    assert_eq!(found.len(), 1, "{recs:?}");
    let (at, prefix) = found[0];
    assert!(at < request_positions(&recs)[0], "{recs:?}");
    let SessionRecord::PrefixApplied {
        agents_md,
        repo_map,
        system_prompt_hash,
        tools_hash,
    } = prefix
    else {
        unreachable!()
    };
    let reader = run.reader();
    let req = &mock.requests()[0];
    let agents = blob_text(&reader, agents_md.as_ref().expect("AGENTS.md was loaded"));
    assert!(agents.contains("Use pnpm, never npm."), "{agents}");
    assert!(text_of(req, 1).contains(agents.trim()), "{agents}");
    let map = blob_text(&reader, repo_map.as_ref().expect("the repo map was built"));
    assert!(map.contains("lib.rs"), "{map}");
    assert!(text_of(req, 3).contains(map.trim()), "{map}");
    assert_eq!(*system_prompt_hash, hex(text_of(req, 0).as_bytes()));
    assert_eq!(*tools_hash, hex(&serde_json::to_vec(&req.tools).unwrap()));
}

#[tokio::test]
async fn prefix_applied_relogged_only_when_inputs_change() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "a.txt"}))]),
        text_step("one"),
        text_step("two"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("first").await;
    run.turn_with("second").await;
    let recs = run.records();
    assert_eq!(request_positions(&recs).len(), 3);
    // Three requests, unchanged inputs: one record. No AGENTS.md and no repo map here.
    let found = prefixes(&recs);
    assert_eq!(found.len(), 1, "{recs:?}");
    assert!(matches!(
        found[0].1,
        SessionRecord::PrefixApplied {
            agents_md: None,
            repo_map: None,
            ..
        }
    ));

    // A session whose inputs differ logs its own.
    fx.write("AGENTS.md", "Prefer small diffs.\n");
    let mock = MockProvider::new(vec![text_step("three")]);
    let mut other = start(fx.builder(config(Mode::Ask), &mock));
    other.turn_with("third").await;
    let recs2 = other.records();
    let found2 = prefixes(&recs2);
    assert_eq!(found2.len(), 1);
    assert_ne!(found2[0].1, found[0].1);
}

#[tokio::test]
async fn request_record_has_turn_step_model_and_block_hashes() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "a.txt"}))]),
        text_step("one"),
        text_step("two"),
    ]);
    let mut run = start(fx.builder(config(Mode::Ask), &mock));
    run.turn_with("first").await;
    run.turn_with("second").await;
    let reqs = mock.requests();
    let got: Vec<_> = run
        .records()
        .into_iter()
        .filter_map(|r| match r {
            SessionRecord::Request {
                hash,
                turn,
                step,
                model,
                blocks,
            } => Some((hash, turn, step, model, blocks)),
            _ => None,
        })
        .collect();
    assert_eq!(got.len(), 3);
    // (user turn, step). A step is the context turn the request builds (spec 7.2's
    // counter, the one `Edit.turn` and the read tracker use); each user message is a
    // context turn of its own, so the first step is 2.
    let numbers: Vec<(u32, u32)> = got.iter().map(|g| (g.1, g.2)).collect();
    assert_eq!(numbers, [(1, 2), (1, 3), (2, 5)]);
    for ((hash, _, _, model, blocks), req) in got.iter().zip(&reqs) {
        assert_eq!(model.as_str(), MODEL);
        assert_eq!(hash.as_str(), hex(&serde_json::to_vec(req).unwrap()));
        let stable = hex(&serde_json::to_vec(&(&req.system, &req.tools)).unwrap());
        assert_eq!(blocks.stable, stable);
        // The state tail is the last message when there is one (none on the first step).
        let tail = req.messages.last().filter(|_| req.volatile_tail);
        assert_eq!(blocks.tail, hex(&serde_json::to_vec(&tail).unwrap()));
        for h in [
            &blocks.stable,
            &blocks.pinned,
            &blocks.history,
            &blocks.tail,
        ] {
            assert_eq!(h.len(), 64, "{h}");
        }
    }
    // The prefix did not change: `stable` (and the empty pinned block) are identical.
    assert!(got.iter().all(|g| g.4.stable == got[0].4.stable));
    assert!(got.iter().all(|g| g.4.pinned == got[0].4.pinned));
    // The conversation grew at every step; after the read, the state tail lists the file.
    assert_ne!(got[0].4.history, got[1].4.history);
    assert_ne!(got[1].4.history, got[2].4.history);
    assert!(reqs[1].volatile_tail);
    assert_ne!(got[0].4.tail, got[1].4.tail);
}

#[tokio::test]
async fn edit_records_carry_post_image_equal_to_disk() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![call("r1", "read", json!({"path": "a.txt"}))]),
        tools_step(vec![call(
            "e1",
            "edit",
            json!({"path": "a.txt", "old_text": "alpha", "new_text": "beta"}),
        )]),
        tools_step(vec![call(
            "e2",
            "edit",
            json!({"path": "a.txt", "old_text": "beta", "new_text": "gamma"}),
        )]),
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "new.txt", "content": "fresh\n"}),
        )]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::AutoEdit), &mock));
    assert_eq!(run.turn_with("edit").await, FinishReason::Completed);
    let reader = run.reader();
    let recs = run.records();
    let edits: Vec<_> = recs
        .iter()
        .filter_map(|r| match r {
            SessionRecord::Edit {
                path,
                pre_image,
                post_image_blob,
                ..
            } => Some((path.as_str().to_string(), pre_image, post_image_blob)),
            _ => None,
        })
        .collect();
    assert_eq!(edits.len(), 3, "{recs:?}");
    let read = |b: &Option<BlobRef>| b.as_ref().map(|b| blob_text(&reader, b));
    assert_eq!(edits[0].0, "a.txt");
    assert_eq!(read(edits[0].1).as_deref(), Some("alpha\n"));
    assert_eq!(read(edits[0].2).as_deref(), Some("beta\n"));
    // One edit's post-image is the next one's pre-image: the same blob.
    assert_eq!(edits[1].1, edits[0].2);
    assert_eq!(read(edits[1].2).unwrap(), fx_read(&fx, "a.txt"));
    assert_eq!(edits[2].0, "new.txt");
    assert_eq!(*edits[2].1, None);
    assert_eq!(read(edits[2].2).unwrap(), fx_read(&fx, "new.txt"));

    // The result's effect carries the same post-image.
    let post_in_effect = recs.iter().find_map(|r| match r {
        SessionRecord::ToolResult { id, effects, .. } if id == "w1" => {
            effects.iter().find_map(|e| match e {
                PersistedEffect::Edited {
                    post_image_blob, ..
                } => post_image_blob.clone(),
                _ => None,
            })
        }
        _ => None,
    });
    assert_eq!(post_in_effect.as_ref(), edits[2].2.as_ref());
}

fn fx_read(fx: &Fx, rel: &str) -> String {
    std::fs::read_to_string(fx.dir.path().join(rel)).unwrap()
}

#[tokio::test]
async fn approval_request_logged_with_call_id_before_event() {
    let fx = Fx::new();
    let mock = MockProvider::new(vec![
        tools_step(vec![call(
            "w1",
            "write",
            json!({"path": "out.txt", "content": "x"}),
        )]),
        text_step("ok"),
    ]);
    let mut run = start(EngineBuilder {
        interactive: true,
        ..fx.builder(config(Mode::Ask), &mock)
    });
    run.say("write it").await;
    let events = run
        .until(|e| matches!(e, Event::ToolApprovalRequested { .. }))
        .await;
    let Some(Event::ToolApprovalRequested {
        request_id, reason, ..
    }) = events.last()
    else {
        unreachable!()
    };
    // Read from disk the moment the event arrives.
    let recs = run.records();
    assert_eq!(
        recs.last(),
        Some(&SessionRecord::ApprovalRequested {
            request_id: request_id.clone(),
            kind: ApprovalKind::Tool,
            call_id: Some("w1".into()),
            reason: reason.clone(),
        }),
        "{recs:?}"
    );
    run.engine
        .send(Command::Deny {
            request_id: request_id.clone(),
            reason: None,
        })
        .await
        .unwrap();
    run.until(|e| matches!(e, Event::TurnFinished { .. })).await;
}

#[tokio::test]
async fn persisted_paths_are_root_relative() {
    let fx = Fx::new();
    fx.write("a.txt", "alpha\n");
    let mock = MockProvider::new(vec![
        tools_step(vec![
            call("r1", "read", json!({"path": "a.txt"})),
            call(
                "w1",
                "write",
                json!({"path": "sub/b.txt", "content": "b\n"}),
            ),
        ]),
        text_step("ok"),
    ]);
    let mut run = start(fx.builder(config(Mode::AutoEdit), &mock));
    assert_eq!(run.turn_with("go").await, FinishReason::Completed);
    let recs = run.records();
    let paths: Vec<String> = recs
        .iter()
        .flat_map(|r| match r {
            SessionRecord::ToolResult { effects, .. } => effects
                .iter()
                .filter_map(|e| e.path().map(|p| p.as_str().to_string()))
                .collect(),
            SessionRecord::Edit { path, .. } => vec![path.as_str().to_string()],
            _ => vec![],
        })
        .collect();
    assert_eq!(paths, ["a.txt", "sub/b.txt", "sub/b.txt"], "{recs:?}");
    // No record line names the root (only the header does).
    let root = fx.root().display().to_string();
    let given = fx.dir.path().display().to_string();
    let text = std::fs::read_to_string(run.engine.session_path()).unwrap();
    let mut lines = text.lines();
    assert!(lines.next().unwrap().contains(&root));
    for line in lines {
        assert!(!line.contains(&root) && !line.contains(&given), "{line}");
    }
}

#[tokio::test]
async fn config_hash_stable_for_identical_config() {
    let fx = Fx::new();
    let header = |cfg: Config| {
        let mock = MockProvider::new(vec![]);
        let run = start(fx.builder(cfg, &mock));
        run.reader().header().clone()
    };
    let a = header(config(Mode::Ask));
    let b = header(config(Mode::Ask));
    let mut changed = config(Mode::Ask);
    changed.max_turns = 7;
    let c = header(changed);
    let ha = a.config_hash.clone().expect("a config hash");
    assert_eq!(ha.len(), 64);
    assert!(ha.bytes().all(|x| x.is_ascii_hexdigit()));
    assert_eq!(b.config_hash.as_deref(), Some(ha.as_str()));
    assert_ne!(c.config_hash.as_deref(), Some(ha.as_str()));
    assert_ne!(a.session_id, b.session_id);
    // The rest of the header the engine fills.
    assert_eq!(a.model.as_deref(), Some(MODEL));
    assert_eq!(a.project, paths::project_id(&fx.root()));
    assert!(!a.tautly_version.is_empty());
}

#[test]
fn config_hash_is_a_canonical_serialization_independent_of_key_order() {
    let one: Config = toml::from_str(
        r#"
model = "mock/test"
max_turns = 40
pins = ["first", "second"]
[permissions]
mode = "auto-edit"
allow = ["bash(cargo test*)"]
[budget.session]
max_usd = 2.5
[catalog_overrides.models."b/two"]
context_window = 1000
max_output = 10
parallel_tools = false
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
[catalog_overrides.models."a/one"]
context_window = 2000
max_output = 20
parallel_tools = true
reasoning = "none"
cache = "none"
images = false
edit_format = "str_replace"
"#,
    )
    .unwrap();
    let two: Config = toml::from_str(
        r#"
pins = ["first", "second"]
max_turns = 40
model = "mock/test"
[catalog_overrides.models."a/one"]
edit_format = "str_replace"
images = false
cache = "none"
reasoning = "none"
parallel_tools = true
max_output = 20
context_window = 2000
[catalog_overrides.models."b/two"]
edit_format = "str_replace"
images = false
cache = "none"
reasoning = "none"
parallel_tools = false
max_output = 10
context_window = 1000
[budget.session]
max_usd = 2.5
[permissions]
allow = ["bash(cargo test*)"]
mode = "auto-edit"
"#,
    )
    .unwrap();
    assert_eq!(one.hash(), two.hash());
    // It is the hash of the canonical JSON (object keys sorted), not of a Debug form.
    let canonical = serde_json::to_vec(&serde_json::to_value(&one).unwrap()).unwrap();
    assert_eq!(one.hash(), hex(&canonical));
    // Pin order matters (pins are rendered in order).
    let mut swapped = one.clone();
    swapped.pins.reverse();
    assert_ne!(swapped.hash(), one.hash());
}

#[test]
fn labeled_record_renames_session() {
    let (data, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let init = HeaderInit {
        label: Some("first".into()),
        ..HeaderInit::default()
    };
    let mut w =
        SessionWriter::<SessionRecord>::create_in(data.path(), project.path(), init).unwrap();
    let path = w.path().to_path_buf();
    let label = || session_label(&SessionReader::open(&path).unwrap()).unwrap();
    assert_eq!(label().as_deref(), Some("first"));
    for name in ["second", "third"] {
        w.append(&SessionRecord::Labeled { label: name.into() })
            .unwrap();
        w.append(&SessionRecord::PinsFolded).unwrap();
    }
    w.checkpoint().unwrap();
    assert_eq!(label().as_deref(), Some("third"));

    let plain = SessionWriter::<SessionRecord>::create_in(
        data.path(),
        project.path(),
        HeaderInit::default(),
    )
    .unwrap();
    assert_eq!(
        session_label(&SessionReader::open(plain.path()).unwrap()).unwrap(),
        None
    );
}
