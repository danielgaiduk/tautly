//! The Obsidian-compatible file format (spec 7.4): YAML properties, tags, readable file
//! names, `[[wikilinks]]` followed one hop by recall, and old files still loading.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Value, json};
use tautly_index::Index;
use tautly_memory::{
    HitSource, MemAnchor, MemoryChange, MemoryDraft, MemoryEntry, MemoryKind, MemoryStore,
};
use tautly_sandbox::LocalSandbox;
use tempfile::TempDir;

fn est(s: &str) -> u32 {
    s.len().div_ceil(4) as u32
}

struct Env {
    project: TempDir,
    data: TempDir,
    _db: TempDir,
    index: Arc<Index>,
    store: MemoryStore,
}

async fn setup(files: &[(&str, &str)]) -> Env {
    let project = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();
    for (rel, content) in files {
        let p = project.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    let sb = Arc::new(LocalSandbox::new(project.path().to_path_buf()).unwrap());
    let index = Arc::new(Index::open(&db.path().join("index.db"), sb).unwrap());
    index.refresh().await.unwrap();
    let store = MemoryStore::open_in(project.path(), data.path(), index.clone()).unwrap();
    Env {
        project,
        data,
        _db: db,
        index,
        store,
    }
}

impl Env {
    fn dir(&self) -> PathBuf {
        self.project.path().join(".tautly/memory")
    }

    fn reopen(&self) -> MemoryStore {
        MemoryStore::open_in(self.project.path(), self.data.path(), self.index.clone()).unwrap()
    }

    /// The `.md` file names in the memory directory, sorted.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".md"))
            .collect();
        names.sort();
        names
    }
}

fn draft(title: &str, body: &str) -> MemoryDraft {
    MemoryDraft {
        id: None,
        title: title.into(),
        kind: MemoryKind::Fact,
        tags: vec![],
        anchors: vec![],
        body: body.into(),
    }
}

fn add(m: &mut MemoryStore, d: MemoryDraft) -> String {
    let change = m.draft_to_change(d, &est).unwrap();
    let id = change.upsert.as_ref().unwrap().id.clone();
    m.apply(&change).unwrap();
    id
}

fn golden_entry() -> MemoryEntry {
    MemoryEntry {
        id: "use-pnpm-1a2b".into(),
        title: "Use pnpm, not npm".into(),
        kind: MemoryKind::Convention,
        tags: vec!["build".into(), "js/tooling".into()],
        anchors: vec![
            MemAnchor {
                path: PathBuf::from("package.json"),
                symbol: None,
                hash: "3f2a9c".into(),
            },
            MemAnchor {
                path: PathBuf::from("src/config.rs"),
                symbol: Some("Config::load".into()),
                hash: "b7e1".into(),
            },
        ],
        created: UNIX_EPOCH + Duration::from_secs(1_793_700_000),
        verified: Some(UNIX_EPOCH + Duration::from_secs(1_793_703_600)),
        body: "Run `pnpm install`. See [[Lockfile policy]] and [[ci-cache|the CI cache]].".into(),
        links: vec!["Lockfile policy".into(), "ci-cache".into()],
        stale: false,
    }
}

const GOLDEN: &str = "---
id: use-pnpm-1a2b
title: \"Use pnpm, not npm\"
kind: convention
tags:
  - build
  - js/tooling
anchors:
  - path: package.json
    hash: \"3f2a9c\"
  - path: src/config.rs
    symbol: \"Config::load\"
    hash: \"b7e1\"
created: 2026-11-03T10:00:00
verified: 2026-11-03T11:00:00
---
Run `pnpm install`. See [[Lockfile policy]] and [[ci-cache|the CI cache]].
";

#[test]
fn vault_files_parse_as_obsidian_properties() {
    let e = golden_entry();
    assert_eq!(e.to_markdown(), GOLDEN);

    // Plain YAML a properties reader takes: every key at the top level, tags a list of
    // strings, dates as ISO strings, anchors a list of maps.
    let front = GOLDEN
        .strip_prefix("---\n")
        .unwrap()
        .split("\n---\n")
        .next()
        .unwrap();
    let props: Value = serde_saphyr::from_str(front).unwrap();
    assert_eq!(
        props,
        json!({
            "id": "use-pnpm-1a2b",
            "title": "Use pnpm, not npm",
            "kind": "convention",
            "tags": ["build", "js/tooling"],
            "anchors": [
                {"path": "package.json", "hash": "3f2a9c"},
                {"path": "src/config.rs", "symbol": "Config::load", "hash": "b7e1"},
            ],
            "created": "2026-11-03T10:00:00",
            "verified": "2026-11-03T11:00:00",
        })
    );
    // And back, links parsed from the body.
    assert_eq!(MemoryEntry::from_markdown(GOLDEN).unwrap(), e);

    // Values that YAML would misread are quoted, and still come back unchanged.
    let tricky = MemoryEntry {
        title: "true".into(),
        tags: vec!["2026".into()],
        anchors: vec![MemAnchor {
            path: PathBuf::from("a: b #c.rs"),
            symbol: Some("- x".into()),
            hash: "00".into(),
        }],
        body: "plain".into(),
        links: vec![],
        verified: None,
        ..golden_entry()
    };
    let text = tricky.to_markdown();
    assert!(text.contains("title: \"true\"\n"), "{text}");
    let back = MemoryEntry::from_markdown(&text).unwrap();
    assert_eq!(back.title, "true");
    assert_eq!(back.anchors, tricky.anchors);
    assert_eq!(back.verified, None);

    // Hand edits in Obsidian: a single tag string, `#` prefixes, a date without time, a
    // UTC `Z`, unknown properties (aliases, cssclasses) are all fine.
    let edited = MemoryEntry::from_markdown(
        "---\nid: x-1\ntitle: Hand made\nkind: gotcha\ntags: \"#Build\"\naliases:\n  - hm\n\
         cssclasses: [wide]\ncreated: 2026-11-03\nverified: 2026-11-03T11:00:00Z\n---\nBody.\n",
    )
    .unwrap();
    assert_eq!(edited.tags, ["Build"]);
    assert_eq!(
        edited.created,
        UNIX_EPOCH + Duration::from_secs(1_793_664_000)
    );
    assert_eq!(edited.verified, golden_entry().verified);
}

#[tokio::test]
async fn files_are_named_by_title_with_numeric_suffixes_and_keep_tags() {
    let mut e = setup(&[]).await;
    let mut d = draft("Use pnpm, not npm!", "Always pnpm.");
    d.tags = vec![
        "#build".into(),
        "js tooling".into(),
        "build".into(),
        "!!".into(),
    ];
    let a = add(&mut e.store, d);
    let b = add(&mut e.store, draft("Use pnpm, not npm!", "Again."));
    let c = add(&mut e.store, draft("use PNPM not npm", "Third."));
    assert_eq!(
        e.files(),
        [
            "use-pnpm-not-npm-2.md",
            "use-pnpm-not-npm-3.md",
            "use-pnpm-not-npm.md"
        ]
    );
    assert_eq!(e.store.file_name(&a), Some("use-pnpm-not-npm.md"));
    assert_eq!(e.store.file_name(&b), Some("use-pnpm-not-npm-2.md"));
    assert_eq!(e.store.file_name(&c), Some("use-pnpm-not-npm-3.md"));
    let m = e.reopen();
    let got = m.entries().iter().find(|x| x.id == a).unwrap();
    assert_eq!(got.tags, ["build", "js-tooling"]);
    // An update by id keeps its file; a delete removes it.
    let mut upd = draft("A new title", "Changed.");
    upd.id = Some(b.clone());
    add(&mut e.store, upd);
    assert_eq!(e.store.file_name(&b), Some("use-pnpm-not-npm-2.md"));
    e.store
        .apply(&MemoryChange {
            upsert: None,
            delete: vec![c],
        })
        .unwrap();
    assert_eq!(e.files(), ["use-pnpm-not-npm-2.md", "use-pnpm-not-npm.md"]);
    // A title with no usable characters still gets a name.
    let odd = add(&mut e.store, draft("!!!", "x"));
    assert_eq!(e.store.file_name(&odd), Some("entry.md"));
    // Non-ASCII letters are kept, as Obsidian shows them.
    let jp = add(&mut e.store, draft("日本語 メモ", "x"));
    assert_eq!(e.store.file_name(&jp), Some("日本語-メモ.md"));
}

#[tokio::test]
async fn wikilinks_are_followed_one_hop_by_recall() {
    let mut e = setup(&[]).await;
    let a = add(
        &mut e.store,
        draft(
            "Release checklist",
            "Zebra step: bump versions. See [[Build cache]], [[Nowhere]] and [[Build cache#Why|why]].",
        ),
    );
    let b = add(
        &mut e.store,
        draft(
            "Build cache",
            "Artifacts live in target/. Two hops away: [[Deep note]].",
        ),
    );
    let c = add(&mut e.store, draft("Deep note", "Not followed."));
    let a_entry = e.store.entries().iter().find(|x| x.id == a).unwrap();
    // Unknown targets are kept as written; duplicates once; the heading is not the target.
    assert_eq!(a_entry.links, ["Build cache", "Nowhere"]);

    let hits = e.store.search("zebra", 5, 2000, &est);
    let sources: Vec<&HitSource> = hits.iter().map(|h| &h.source).collect();
    assert_eq!(
        sources,
        [
            &HitSource::Project(a.clone()),
            &HitSource::Project(b.clone())
        ],
        "{hits:?}"
    );
    assert_eq!(hits[0].linked_from, None);
    assert_eq!(hits[1].linked_from.as_deref(), Some(a.as_str()));
    assert!(hits[1].text.contains("Artifacts"));
    assert!(
        !hits
            .iter()
            .any(|h| h.source == HitSource::Project(c.clone()))
    );

    // A linked entry that is a hit of its own is not repeated; the hop is from each match,
    // so the match B now brings in C.
    let both = e.store.search("zebra artifacts", 5, 2000, &est);
    let ids: Vec<(&HitSource, Option<&str>)> = both
        .iter()
        .map(|h| (&h.source, h.linked_from.as_deref()))
        .collect();
    assert_eq!(
        ids,
        [
            (&HitSource::Project(b.clone()), None),
            (&HitSource::Project(a.clone()), None),
            (&HitSource::Project(c.clone()), Some(b.as_str())),
        ]
    );

    // Linked entries share the cap: with no room left after the hit, none is added.
    let a_cost = {
        let h = &hits[0];
        est(&h.title) + est(&h.text)
    };
    let tight = e.store.search("zebra", 5, a_cost, &est);
    assert_eq!(tight.len(), 1, "{tight:?}");
    let total: u32 = hits.iter().map(|h| est(&h.title) + est(&h.text)).sum();
    assert!(total <= 2000);
}

#[tokio::test]
async fn renamed_file_keeps_links_by_id() {
    let mut e = setup(&[]).await;
    let b = add(
        &mut e.store,
        draft("Build cache", "Artifacts live in target/."),
    );
    let a = add(
        &mut e.store,
        draft(
            "Release checklist",
            &format!("Zebra step. See [[{b}|the cache]]."),
        ),
    );
    // Written as a link Obsidian resolves: by file name, the label kept.
    let a_file = e.dir().join(e.store.file_name(&a).unwrap());
    let a_text = fs::read_to_string(&a_file).unwrap();
    assert!(a_text.contains("[[build-cache|the cache]]"), "{a_text}");
    // Obsidian renames the file and updates the links to it.
    let old = e.dir().join(e.store.file_name(&b).unwrap());
    fs::rename(&old, e.dir().join("Where artifacts go.md")).unwrap();
    fs::write(
        &a_file,
        a_text.replace("[[build-cache|", "[[Where artifacts go|"),
    )
    .unwrap();
    // A note a teammate wrote by hand links by id, which no rename breaks.
    fs::write(
        e.dir().join("hand-note.md"),
        format!(
            "---\nid: hand-note\ntitle: Hand note\nkind: fact\ncreated: 2026-01-01\n---\n\
             Quokka facts, see [[{b}]].\n"
        ),
    )
    .unwrap();

    let m = e.reopen();
    assert_eq!(m.file_name(&b), Some("Where artifacts go.md"));
    let hits = m.search("zebra", 5, 2000, &est);
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[1].source, HitSource::Project(b.clone()));
    assert_eq!(hits[1].linked_from.as_deref(), Some(a.as_str()));
    // A git mv updates no links; the link by id still finds the entry.
    fs::rename(
        e.dir().join("Where artifacts go.md"),
        e.dir().join("cache-notes.md"),
    )
    .unwrap();
    let mut m = e.reopen();
    let hits = m.search("quokka", 5, 2000, &est);
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[1].source, HitSource::Project(b.clone()));
    // Updating the entry writes the renamed file, not a new one.
    let mut upd = draft("Build cache", "Artifacts live in target/ and ~/.cache.");
    upd.id = Some(b.clone());
    add(&mut m, upd);
    assert_eq!(e.files().len(), 3, "{:?}", e.files());
    let text = fs::read_to_string(e.dir().join("cache-notes.md")).unwrap();
    assert!(text.contains("~/.cache"), "{text}");
}

/// Obsidian resolves `[[...]]` by file name, so the links of an applied entry that name an
/// entry by title or id are written as `[[<file stem>|label]]` (the label defaults to the
/// linked entry's title); unknown targets and links that already name a file are kept.
#[tokio::test]
async fn applied_links_name_files_so_obsidian_resolves_them() {
    let mut e = setup(&[]).await;
    let pnpm = add(&mut e.store, draft("Use pnpm, not npm", "Always pnpm."));
    let odd = add(&mut e.store, draft("Old | odd [title]", "Old."));
    let body = format!(
        "See [[Use pnpm, not npm]], [[use pnpm, NOT npm#Why]], [[{pnpm}|the rule]], \
         [[use-pnpm-not-npm]], [[Nowhere]], ![[Use pnpm, not npm]], [[{odd}]] \
         and [[Lint rules]]."
    );
    let lint = add(&mut e.store, draft("Lint rules", &body));
    let text = fs::read_to_string(e.dir().join("lint-rules.md")).unwrap();
    let written = text.split("---\n").nth(2).unwrap().trim_end();
    assert_eq!(
        written,
        "See [[use-pnpm-not-npm|Use pnpm, not npm]], \
         [[use-pnpm-not-npm#Why|Use pnpm, not npm]], [[use-pnpm-not-npm|the rule]], \
         [[use-pnpm-not-npm]], [[Nowhere]], ![[use-pnpm-not-npm|Use pnpm, not npm]], \
         [[old-odd-title|Old - odd (title)]] and [[lint-rules|Lint rules]].",
        "{text}"
    );
    // Recall resolves the rewritten links (by file name).
    let entry = e.store.entries().iter().find(|x| x.id == lint).unwrap();
    assert!(
        entry.links.contains(&"use-pnpm-not-npm".to_string()),
        "{:?}",
        entry.links
    );
    let hits = e.reopen().search("lint", 5, 2000, &est);
    assert!(
        hits.iter()
            .any(|h| h.source == HitSource::Project(pnpm.clone())
                && h.linked_from.as_deref() == Some(lint.as_str())),
        "{hits:?}"
    );
    // A link to an entry the same change deletes is kept as written.
    let merged = MemoryChange {
        upsert: Some(MemoryEntry {
            body: format!("Merged: [[Use pnpm, not npm]] and [[{odd}|odd]]."),
            ..e.store
                .draft_to_change(draft("Merged", "m"), &est)
                .unwrap()
                .upsert
                .unwrap()
        }),
        delete: vec![pnpm.clone()],
    };
    e.store.apply(&merged).unwrap();
    let text = fs::read_to_string(e.dir().join("merged.md")).unwrap();
    assert!(
        text.contains("Merged: [[Use pnpm, not npm]] and [[old-odd-title|odd]]."),
        "{text}"
    );
}

#[tokio::test]
async fn json_valued_frontmatter_loads_and_non_yaml_is_skipped() {
    let e = setup(&[("lib.rs", "fn a(){1}")]).await;
    // `key: <JSON>` lines are YAML (flow style), `<slug>-<hex>.md` a valid name.
    let old = "---\nid: \"use-pnpm-1a2b\"\ntitle: \"Use pnpm: always\"\nkind: \"convention\"\n\
               anchors: [{\"path\":\"lib.rs\",\"hash\":\"abc\"},{\"path\":\"lib.rs\",\"symbol\":\"a\",\"hash\":\"def\"}]\n\
               created: \"2026-01-01T00:00:00Z\"\nverified: \"2026-01-02T00:00:00Z\"\n---\nAlways pnpm.\n";
    fs::create_dir_all(e.dir()).unwrap();
    fs::write(e.dir().join("use-pnpm-1a2b.md"), old).unwrap();
    // A plain scalar YAML cannot read (`: ` inside): skipped, not half-read.
    fs::write(
        e.dir().join("hand-0001.md"),
        "---\nid: hand-0001\ntitle: Note: colon\nkind: fact\ncreated: 2026-01-01T00:00:00Z\n---\nb\n",
    )
    .unwrap();
    let mut m = e.reopen();
    assert_eq!(m.skipped_count(), 1, "{:?}", m.skipped());
    assert!(
        m.skipped()[0].1.contains("not valid YAML"),
        "{:?}",
        m.skipped()
    );
    let got = m
        .entries()
        .iter()
        .find(|x| x.id == "use-pnpm-1a2b")
        .unwrap();
    assert_eq!(got.title, "Use pnpm: always");
    assert_eq!(got.kind, MemoryKind::Convention);
    assert_eq!(got.anchors.len(), 2);
    assert_eq!(got.anchors[1].symbol.as_deref(), Some("a"));
    assert!(got.tags.is_empty());
    assert_eq!(
        got.verified,
        Some(UNIX_EPOCH + Duration::from_secs(1_767_312_000))
    );
    assert!(m.entries().iter().all(|x| x.id != "hand-0001"));
    // Rewritten in place, in the new format, when updated.
    let mut upd = draft("Use pnpm: always", "Always pnpm, really.");
    upd.id = Some("use-pnpm-1a2b".into());
    add(&mut m, upd);
    assert_eq!(e.files(), ["hand-0001.md", "use-pnpm-1a2b.md"]);
    let text = fs::read_to_string(e.dir().join("use-pnpm-1a2b.md")).unwrap();
    assert!(text.starts_with("---\nid: use-pnpm-1a2b\n"), "{text}");
    assert!(text.contains("\nanchors:\n  - path: lib.rs\n"), "{text}");
}

#[tokio::test]
async fn duplicate_ids_and_unsafe_names_are_skipped() {
    let mut e = setup(&[]).await;
    let id = add(&mut e.store, draft("Good", "ok"));
    let text = fs::read_to_string(e.dir().join("good.md")).unwrap();
    fs::write(e.dir().join("zz-copy.md"), &text).unwrap();
    fs::write(e.dir().join(".hidden.md"), &text).unwrap();
    let m = e.reopen();
    assert_eq!(m.entries().len(), 1);
    assert_eq!(m.file_name(&id), Some("good.md"));
    assert_eq!(m.skipped_count(), 1, "{:?}", m.skipped());
    assert!(m.skipped()[0].1.contains("good.md"), "{:?}", m.skipped());
}

/// Hostile frontmatter (a repository is untrusted) is refused or read, never a panic or
/// a hang.
#[test]
fn hostile_frontmatter_is_bounded() {
    let laughs = {
        let mut s = String::from(
            "a: &a [\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\",\"lol\"]\n",
        );
        let mut prev = 'a';
        for c in 'b'..='j' {
            s.push_str(&format!(
                "{c}: &{c} [*{prev},*{prev},*{prev},*{prev},*{prev},*{prev},*{prev},*{prev}]\n"
            ));
            prev = c;
        }
        s
    };
    let deep = format!("id: {}x{}\n", "[".repeat(5000), "]".repeat(5000));
    let block_deep: String = (0..3000)
        .map(|i| format!("{}k:\n", " ".repeat(i)))
        .collect();
    let cases = [
        laughs,
        deep,
        block_deep,
        "id: !!binary AAAA\n".to_string(),
        "id: \"\\uD800\"\n".to_string(),
        "? [a, b]\n: c\n".to_string(),
        "id: a\nid: b\n".to_string(),
        "\t\t- : : :\n".to_string(),
        "%YAML 9.9\n--- x\n".to_string(),
        "title: [unclosed\n".to_string(),
        "\u{0}\u{1}\u{feff}".to_string(),
    ];
    for front in cases {
        let file =
            format!("---\n{front}title: t\nkind: fact\ncreated: 2026-01-01\nid: ok-1\n---\nbody\n");
        let t = Instant::now();
        let parsed = std::panic::catch_unwind(|| MemoryEntry::from_markdown(&file));
        assert!(parsed.is_ok(), "panicked on {front:?}");
        assert!(t.elapsed() < Duration::from_secs(2), "slow on {front:?}");
    }
}
