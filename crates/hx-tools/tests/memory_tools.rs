use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use hx_index::Index;
use hx_memory::{MemoryDraft, MemoryKind, MemoryStore};
use hx_provider::EditFormat;
use hx_sandbox::{Effect, LocalSandbox, Sandbox};
use hx_tools::{ContextOp, RecallTool, RememberTool, Tool, ToolCx, ToolResult, parse_context_op};
use serde_json::{Value, json};
use tempfile::TempDir;

fn est(s: &str) -> u32 {
    s.len().div_ceil(4) as u32
}

struct Env {
    project: TempDir,
    data: TempDir,
    _db: TempDir,
    sb: LocalSandbox,
    index: Arc<Index>,
    store: Arc<RwLock<MemoryStore>>,
    cx: ToolCx,
}

async fn env(files: &[(&str, &str)]) -> Env {
    let project = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();
    let sb = LocalSandbox::new(project.path().to_path_buf()).unwrap();
    for (rel, content) in files {
        sb.write_atomic(Path::new(rel), content.as_bytes(), None)
            .await
            .unwrap();
    }
    let index_sb = Arc::new(LocalSandbox::new(project.path().to_path_buf()).unwrap());
    let index = Arc::new(Index::open(&db.path().join("index.db"), index_sb).unwrap());
    index.refresh().await.unwrap();
    let store = MemoryStore::open_in(project.path(), data.path(), index.clone()).unwrap();
    let store = Arc::new(RwLock::new(store));
    let cx = ToolCx::new(EditFormat::StrReplace, project.path().join(".spill"))
        .with_index(index.clone())
        .with_memory(store.clone());
    Env {
        project,
        data,
        _db: db,
        sb,
        index,
        store,
        cx,
    }
}

async fn run(e: &Env, tool: &dyn Tool, args: Value) -> ToolResult {
    tool.run(args, &e.sb, &e.cx).await
}

async fn remember(e: &Env, args: Value) -> ToolResult {
    run(e, &RememberTool, args).await
}

/// Write `title`/`body` (anchored on `lib.rs`'s symbol `a` when `anchored`) straight into
/// the store, as an approved proposal would.
fn add(e: &Env, title: &str, body: &str, anchored: bool) -> String {
    let anchors = if anchored {
        vec![(PathBuf::from("lib.rs"), Some("a".to_string()))]
    } else {
        vec![]
    };
    let draft = MemoryDraft {
        id: None,
        title: title.into(),
        kind: MemoryKind::Fact,
        anchors,
        body: body.into(),
    };
    let mut store = e.store.write().unwrap();
    let change = store.draft_to_change(draft, &est).unwrap();
    let id = change.upsert.as_ref().unwrap().id.clone();
    store.apply(&change).unwrap();
    id
}

#[tokio::test]
async fn remember_tool_emits_context_op_and_writes_nothing() {
    let e = env(&[("lib.rs", "fn a(){1}")]).await;
    let args = json!({
        "title": "Use pnpm",
        "kind": "convention",
        "body": "Run pnpm install, never npm.",
        "anchors": [{"path": "lib.rs", "symbol": "a"}, {"path": "lib.rs"}]
    });
    assert_eq!(RememberTool.effect(&args).unwrap(), Effect::ReadOnly);
    let r = run(&e, &RememberTool, args.clone()).await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(r.output, "proposed; awaiting user approval");
    assert!(r.effects.is_empty());

    let want = MemoryDraft {
        id: None,
        title: "Use pnpm".into(),
        kind: MemoryKind::Convention,
        anchors: vec![
            (PathBuf::from("lib.rs"), Some("a".into())),
            (PathBuf::from("lib.rs"), None),
        ],
        body: "Run pnpm install, never npm.".into(),
    };
    assert_eq!(
        parse_context_op("remember", &args),
        Some(ContextOp::Remember(want))
    );
    // Nothing on disk, nothing in the store.
    assert!(!e.project.path().join(".hx").exists());
    assert!(!e.data.path().join("memory").exists());
    assert!(e.store.read().unwrap().entries().is_empty());

    // An update names the entry.
    let with_id = json!({"id": "use-pnpm-0001", "title": "t", "kind": "fact", "body": "b"});
    assert!(matches!(
        parse_context_op("remember", &with_id),
        Some(ContextOp::Remember(MemoryDraft { id: Some(id), .. })) if id == "use-pnpm-0001"
    ));
}

#[tokio::test]
async fn remember_rejects_invalid_drafts_so_the_model_can_fix_them() {
    let e = env(&[("lib.rs", "fn a(){1}")]).await;
    let base = json!({"title": "T", "kind": "fact", "body": "b"});

    let mut long = base.clone();
    long["body"] = json!("x".repeat(1300));
    let r = remember(&e, long).await;
    assert!(r.is_error && r.output.contains("300"), "{}", r.output);

    let mut unindexed = base.clone();
    unindexed["anchors"] = json!([{"path": "missing.rs"}]);
    let r = remember(&e, unindexed.clone()).await;
    assert!(
        r.is_error && r.output.contains("missing.rs"),
        "{}",
        r.output
    );
    // The op itself is only the parsed draft; the engine does not apply it after an error.
    assert!(parse_context_op("remember", &unindexed).is_some());

    let mut ghost = base.clone();
    ghost["id"] = json!("no-such-entry");
    let r = remember(&e, ghost).await;
    assert!(
        r.is_error && r.output.contains("no-such-entry"),
        "{}",
        r.output
    );

    for bad in [
        json!({"title": "T", "kind": "weird", "body": "b"}),
        json!({"title": "", "kind": "fact", "body": "b"}),
        json!({"title": "T", "kind": "fact", "body": "  "}),
        json!({"title": "T", "kind": "fact"}),
        json!({"title": "T", "kind": "fact", "body": "b", "extra": 1}),
        json!({"title": "T", "kind": "fact", "body": "b", "anchors": [{"symbol": "a"}]}),
    ] {
        assert!(RememberTool.effect(&bad).is_err(), "{bad}");
        assert_eq!(parse_context_op("remember", &bad), None, "{bad}");
        let r = remember(&e, bad.clone()).await;
        assert!(r.is_error, "{bad}");
    }
}

#[tokio::test]
async fn remember_without_memory_still_proposes() {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    let args = json!({"title": "T", "kind": "gotcha", "body": "b"});
    let r = RememberTool.run(args, &sb, &cx).await;
    assert_eq!(r.output, "proposed; awaiting user approval");
}

#[tokio::test]
async fn recall_tool_flags_stale_hits() {
    let e = env(&[("lib.rs", "fn a(){1}\nfn b(){2}")]).await;
    let stale = add(
        &e,
        "alpha anchored fact",
        "The alpha function returns one.",
        true,
    );
    let fresh = add(
        &e,
        "alpha unanchored fact",
        "The alpha module is documented.",
        false,
    );

    let r = run(&e, &RecallTool, json!({"query": "alpha"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(!r.output.contains("STALE"), "{}", r.output);

    e.sb.write_atomic(Path::new("lib.rs"), b"fn a(){9}\nfn b(){2}", None)
        .await
        .unwrap();
    e.index.reindex_file(Path::new("lib.rs")).await.unwrap();
    let r = run(&e, &RecallTool, json!({"query": "alpha"})).await;
    let out = &r.output;
    let sections: Vec<&str> = out.split("\n\n").collect();
    let of = |id: &str| *sections.iter().find(|s| s.contains(id)).unwrap();
    assert!(of(&stale).contains("STALE"), "{out}");
    assert!(!of(&fresh).contains("STALE"), "{out}");
    assert!(out.contains("The alpha function returns one."), "{out}");
}

#[tokio::test]
async fn recall_tool_returns_top_five_within_2000_tokens() {
    let e = env(&[]).await;
    for i in 0..8 {
        add(
            &e,
            &format!("scheduler note {i}"),
            &"scheduler detail ".repeat(60),
            false,
        );
    }
    for i in 0..4 {
        let body = format!(
            "# scheduler episode {i}\n{}",
            "scheduler history ".repeat(400)
        );
        e.store
            .read()
            .unwrap()
            .write_episode(&format!("sess-{i}"), &body)
            .unwrap();
    }
    let r = run(&e, &RecallTool, json!({"query": "scheduler"})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(est(&r.output) <= 2000, "{} tokens", est(&r.output));
    let numbered = r
        .output
        .lines()
        .filter(|l| {
            l.split_once(". ")
                .is_some_and(|(n, _)| n.parse::<u32>().is_ok())
        })
        .count();
    assert!((1..=5).contains(&numbered), "{numbered}\n{}", r.output);

    let none = run(&e, &RecallTool, json!({"query": "zzzzzz"})).await;
    assert!(!none.is_error);
    assert_eq!(none.output, "no matching memory");
}

#[tokio::test]
async fn recall_needs_memory_and_a_query() {
    let dir = TempDir::new().unwrap();
    let sb = LocalSandbox::new(dir.path().to_path_buf()).unwrap();
    let cx = ToolCx::new(EditFormat::StrReplace, dir.path().join(".spill"));
    let r = RecallTool.run(json!({"query": "x"}), &sb, &cx).await;
    assert!(r.is_error, "{}", r.output);
    assert_eq!(
        RecallTool.effect(&json!({"query": "x"})).unwrap(),
        Effect::ReadOnly
    );
    for bad in [
        json!({}),
        json!({"query": ""}),
        json!({"query": "  "}),
        json!({"query": "x", "k": 1}),
    ] {
        assert!(RecallTool.effect(&bad).is_err(), "{bad}");
    }
}
