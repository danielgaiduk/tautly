use std::path::Path;
use std::sync::Arc;

use hx_context::{Context, STABLE_LEN, TokenEstimator, repo_map};
use hx_index::Index;
use hx_provider::{Block, CacheStyle, Capabilities, EditFormat, ModelId, ReasoningStyle};
use hx_sandbox::LocalSandbox;

const BUDGET: u32 = 400;

struct Repo {
    root: tempfile::TempDir,
    _db: tempfile::TempDir,
    db_path: std::path::PathBuf,
    idx: Index,
}

fn write_all(root: &Path, files: &[(String, String)]) {
    for (rel, content) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
}

fn open(root: &Path, db: &Path) -> Index {
    let sb = Arc::new(LocalSandbox::new(root.to_path_buf()).unwrap());
    Index::open(db, sb).unwrap()
}

async fn repo(files: &[(&str, &str)]) -> Repo {
    let owned: Vec<(String, String)> = files
        .iter()
        .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
        .collect();
    repo_owned(&owned).await
}

async fn repo_owned(files: &[(String, String)]) -> Repo {
    let root = tempfile::tempdir().unwrap();
    write_all(root.path(), files);
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().join("index.db");
    let idx = open(root.path(), &db_path);
    idx.refresh().await.unwrap();
    Repo {
        root,
        _db: db,
        db_path,
        idx,
    }
}

fn est() -> TokenEstimator {
    TokenEstimator::default()
}

fn caps() -> Capabilities {
    Capabilities {
        context_window: 100_000,
        max_output: 8_000,
        parallel_tools: true,
        reasoning: ReasoningStyle::None,
        cache: CacheStyle::Explicit,
        images: false,
        edit_format: EditFormat::StrReplace,
    }
}

fn model() -> ModelId {
    "anthropic/claude-sonnet".into()
}

fn system_text(b: &Block) -> &str {
    match b {
        Block::Text(t) => t,
        other => panic!("not text: {other:?}"),
    }
}

const WIDGET_SRC: &str = "pub struct Widget;\nimpl Widget {\n    pub fn new() -> Widget { Widget }\n}\npub fn helper() {}\n";
const USER_SRC: &str = "fn run() {\n    let w = Widget::new();\n    helper();\n    helper();\n}\n";

#[tokio::test]
async fn map_lists_top_level_dirs_with_counts_and_languages() {
    let r = repo(&[
        ("src/a.rs", WIDGET_SRC),
        ("src/b.rs", USER_SRC),
        ("src/c.ts", "export function f() {}\n"),
        ("web/x.ts", "export function g() {}\n"),
        ("web/y.ts", "export function h() {}\n"),
        ("docs/readme.md", "# hi\n"),
        ("Cargo.toml", "[package]\nname = \"x\"\n"),
    ])
    .await;
    let map = repo_map::build(&r.idx, &est(), BUDGET);

    let lines: Vec<&str> = map.lines().collect();
    assert!(
        lines.contains(&"Languages (7 files): typescript 3, rust 2, toml 1"),
        "{map}"
    );
    assert!(lines.contains(&"src/ 3 (rust 2, typescript 1)"), "{map}");
    assert!(lines.contains(&"web/ 2 (typescript 2)"), "{map}");
    // A directory with no known language shows its count alone.
    assert!(lines.contains(&"docs/ 1"), "{map}");
    // Largest directory first.
    let pos = |needle: &str| map.find(needle).unwrap();
    assert!(pos("src/ 3") < pos("web/ 2") && pos("web/ 2") < pos("docs/ 1"));
}

#[tokio::test]
async fn map_lists_entry_points() {
    let r = repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\", \"tools/cli\"]\nresolver = \"2\"\n",
        ),
        ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ("crates/a/src/lib.rs", "pub fn a() {}\n"),
        ("crates/b/Cargo.toml", "[package]\nname = \"b\"\n"),
        ("crates/b/src/main.rs", "fn main() {}\n"),
        ("tools/cli/Cargo.toml", "[package]\nname = \"cli\"\n"),
        ("src/main.rs", "fn main() {}\n"),
        (
            "package.json",
            r#"{"name":"x","scripts":{"test":"jest","build":"tsc"}}"#,
        ),
    ])
    .await;
    let map = repo_map::build(&r.idx, &est(), BUDGET);
    let lines: Vec<&str> = map.lines().collect();
    assert!(lines.contains(&"main.rs: src, crates/b/src"), "{map}");
    assert!(lines.contains(&"lib.rs: crates/a/src"), "{map}");
    assert!(
        lines.contains(&"workspace: crates/{a,b}, tools/cli"),
        "{map}"
    );
    assert!(lines.contains(&"scripts: build, test"), "{map}");
    assert!(map.contains("Entry points:"), "{map}");
}

#[tokio::test]
async fn oversized_or_malformed_manifests_are_skipped() {
    let big = format!(
        "{{\"scripts\":{{\"build\":\"{}\"}}}}",
        "x".repeat(200 * 1024)
    );
    let r = repo_owned(&[
        ("package.json".to_string(), big),
        (
            "Cargo.toml".to_string(),
            "[workspace\nmembers = ".to_string(),
        ),
        ("src/lib.rs".to_string(), "pub fn a() {}\n".to_string()),
    ])
    .await;
    let map = repo_map::build(&r.idx, &est(), BUDGET);
    assert!(
        !map.contains("scripts:") && !map.contains("workspace:"),
        "{map}"
    );
    assert!(map.contains("lib.rs: src"), "{map}");
}

#[tokio::test]
async fn map_lists_most_referenced_symbols() {
    let r = repo(&[("src/a.rs", WIDGET_SRC), ("src/b.rs", USER_SRC)]).await;
    let top = r.idx.top_referenced(10);
    assert!(top.len() >= 2, "{top:?}");
    let map = repo_map::build(&r.idx, &est(), BUDGET);
    let line = map
        .lines()
        .find_map(|l| l.strip_prefix("Key symbols: "))
        .unwrap_or_else(|| panic!("{map}"));
    let expected: Vec<String> = top.iter().map(|(n, c)| format!("{n} {c}")).collect();
    assert_eq!(line, expected.join(", "));
}

fn large_repo() -> Vec<(String, String)> {
    let mut files = Vec::new();
    let members: Vec<String> = (0..80)
        .map(|i| format!("\"component_module_{i:02}\""))
        .collect();
    files.push((
        "Cargo.toml".to_string(),
        format!("[workspace]\nmembers = [{}]\n", members.join(", ")),
    ));
    let scripts: Vec<String> = (0..60).map(|i| format!("\"script{i:02}\":\"x\"")).collect();
    files.push((
        "package.json".to_string(),
        format!("{{\"scripts\":{{{}}}}}", scripts.join(",")),
    ));
    for i in 0..80 {
        files.push((
            format!("component_module_{i:02}/Cargo.toml"),
            format!("[package]\nname = \"component_module_{i:02}\"\n"),
        ));
        files.push((
            format!("component_module_{i:02}/src/lib.rs"),
            format!("pub fn api{i:02}() {{}}\npub struct Type{i:02};\n"),
        ));
        files.push((
            format!("component_module_{i:02}/src/main.rs"),
            format!("fn main() {{ api{i:02}(); api{i:02}(); let _ = Type{i:02}; }}\n"),
        ));
        files.push((
            format!("component_module_{i:02}/src/mod{i:02}/m.py"),
            "def f(): pass\n".to_string(),
        ));
    }
    files
}

#[tokio::test]
async fn map_never_exceeds_400_tokens_on_large_repo() {
    let r = repo_owned(&large_repo()).await;
    let est = est();
    // The full map is well over budget, so the cap really cuts.
    assert!(est.estimate(&repo_map::build(&r.idx, &est, 100_000)) > BUDGET);
    let map = repo_map::build(&r.idx, &est, BUDGET);
    assert!(
        est.estimate(&map) <= BUDGET,
        "{} tokens\n{map}",
        est.estimate(&map)
    );
    // The highest-value section is never the one dropped.
    assert!(map.starts_with("Languages (322 files):"), "{map}");
    assert!(map.contains("Dirs:"), "{map}");
    // The same holds when calibration has inflated the estimate.
    let mut inflated = TokenEstimator::default();
    inflated.calibrate(100, 200);
    inflated.calibrate(100, 200);
    let map = repo_map::build(&r.idx, &inflated, BUDGET);
    assert!(inflated.estimate(&map) <= BUDGET, "{map}");
    assert!(map.starts_with("Languages"), "{map}");
}

#[tokio::test]
async fn map_drops_lowest_value_sections_first() {
    let r = repo_owned(&large_repo()).await;
    let est = est();
    let full = repo_map::build(&r.idx, &est, 100_000);
    for header in ["Languages", "Dirs:", "Entry points:", "Key symbols:"] {
        assert!(full.contains(header), "{header} missing from\n{full}");
    }
    // Budget for everything but the key symbols: that section goes, the rest stays.
    let key = full.find("Key symbols:").unwrap();
    let without_key = &full[..key];
    let m = repo_map::build(&r.idx, &est, est.estimate(without_key.trim_end()));
    assert!(
        !m.contains("Key symbols:") && m.contains("Entry points:"),
        "{m}"
    );
    // Less budget: entry points shrink or go before the directories do.
    let entry = full.find("Entry points:").unwrap();
    let m = repo_map::build(&r.idx, &est, est.estimate(full[..entry].trim_end()));
    assert!(!m.contains("Entry points:") && m.contains("Dirs:"), "{m}");
    // Even a tiny budget keeps the first section and stays within bounds.
    let m = repo_map::build(&r.idx, &est, 20);
    assert!(est.estimate(&m) <= 20 && m.starts_with("Languages"), "{m}");
    assert_eq!(repo_map::build(&r.idx, &est, 0), "");
}

#[tokio::test]
async fn map_is_deterministic() {
    let mut files = large_repo();
    let a = repo_owned(&files).await;
    files.reverse();
    let b = repo_owned(&files).await;
    let est = est();
    let (ma, mb) = (
        repo_map::build(&a.idx, &est, BUDGET),
        repo_map::build(&b.idx, &est, BUDGET),
    );
    assert!(!ma.is_empty());
    assert_eq!(ma, mb);
    assert_eq!(ma, repo_map::build(&a.idx, &est, BUDGET));
}

#[tokio::test]
async fn map_of_an_empty_repo_is_empty() {
    let r = repo(&[]).await;
    assert_eq!(repo_map::build(&r.idx, &est(), BUDGET), "");
}

#[tokio::test]
async fn map_uses_the_db_loaded_tree_when_the_index_is_not_ready() {
    let r = repo(&[("src/a.rs", WIDGET_SRC), ("src/b.rs", USER_SRC)]).await;
    let ready = repo_map::build(&r.idx, &est(), BUDGET);
    // A second handle on the same database has loaded the tree but not refreshed.
    let cold = open(r.root.path(), &r.db_path);
    assert!(!cold.is_ready());
    let started = std::time::Instant::now();
    let map = repo_map::build(&cold, &est(), BUDGET);
    // It waits ~200 ms for readiness, then gives up rather than blocking startup.
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(map, ready);
}

#[tokio::test]
async fn map_rendered_after_memory_index_in_stable_block() {
    let r = repo(&[("src/a.rs", WIDGET_SRC), ("src/b.rs", USER_SRC)]).await;
    let mut c = Context {
        agents_md: Some("Use tabs.".into()),
        memory_index: Some("- m1: a fact".into()),
        ..Default::default()
    };
    c.init_repo_map(&r.idx);
    let map = c.repo_map.clone().expect("map set");
    assert!(!map.is_empty());
    let (req, status) = c.assemble(&caps(), &[], &model());

    assert_eq!(STABLE_LEN, 4);
    assert_eq!(req.system.len(), STABLE_LEN);
    assert!(system_text(&req.system[1]).contains("Use tabs."));
    assert!(system_text(&req.system[2]).contains("m1: a fact"));
    assert_eq!(
        system_text(&req.system[3]),
        format!("<repo_map>\n{}\n</repo_map>", map.trim())
    );
    assert!(status.breakdown.repo_map > 0);
    let b = status.breakdown;
    assert_eq!(
        status.used,
        b.system + b.tools + b.agents_md + b.memory + b.repo_map + b.pinned + b.state + b.history
    );

    // Without a map the slot stays, as an empty tag.
    let (req, status) = Context::default().assemble(&caps(), &[], &model());
    assert_eq!(system_text(&req.system[3]), "<repo_map></repo_map>");
    assert!(status.breakdown.repo_map > 0 && status.breakdown.repo_map < 10);
}

#[tokio::test]
async fn map_not_regenerated_mid_session() {
    let r = repo(&[("src/a.rs", WIDGET_SRC), ("src/b.rs", USER_SRC)]).await;
    let mut c = Context::default();
    c.init_repo_map(&r.idx);
    let (before, _) = c.assemble(&caps(), &[], &model());

    // The project changes under the session: new files, a refreshed index.
    write_all(
        r.root.path(),
        &[
            ("extra/one.rs".into(), "pub fn extra_one() {}\n".into()),
            ("extra/two.rs".into(), "pub fn extra_two() {}\n".into()),
        ],
    );
    r.idx.refresh().await.unwrap();
    assert_ne!(
        repo_map::build(&r.idx, &c.tokens, BUDGET),
        c.repo_map.clone().unwrap(),
        "the index really changed"
    );

    let (after, _) = c.assemble(&caps(), &[], &model());
    assert_eq!(
        serde_json::to_vec(&before.system[..STABLE_LEN]).unwrap(),
        serde_json::to_vec(&after.system[..STABLE_LEN]).unwrap()
    );
    assert!(!system_text(&after.system[3]).contains("extra/"));
}
