//! The repo map: a few hundred tokens that tell the model the project's shape (languages,
//! directories, entry points, most-referenced symbols) so it needs no exploration steps to
//! get oriented. It lives in the cache-stable prefix, so it is built once per session
//! ([`crate::Context::init_repo_map`]) and never again.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hx_index::{DirInfo, Index, Lang};

use crate::tokens::TokenEstimator;

/// Token budget of the whole map.
pub const BUDGET: u32 = 400;

/// How long [`build`] waits for the index's first refresh before using the tree as loaded
/// from the database.
const READY_WAIT: Duration = Duration::from_millis(200);
const READY_POLL: Duration = Duration::from_millis(10);
/// Directory levels listed below the root.
const DIR_DEPTH: usize = 2;
const MAX_LANGS: usize = 6;
const MAX_DIRS: usize = 16;
const MAX_DIR_LANGS: usize = 3;
const MAX_LIST: usize = 8;
const MAX_MEMBERS: usize = 24;
const MAX_SCRIPTS: usize = 12;
const MAX_SYMBOLS: usize = 20;
/// Manifests (`Cargo.toml`, `package.json`) larger than this are not read.
const MANIFEST_CAP: u64 = 64 * 1024;

/// One part of the map: a heading and items, most valuable first. Items are dropped from the
/// end when the map is over budget.
struct Section {
    head: String,
    /// Items share the heading's line, comma-separated; otherwise one per line below it.
    inline: bool,
    items: Vec<String>,
}

impl Section {
    fn render(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        Some(if self.inline {
            format!("{} {}", self.head, self.items.join(", "))
        } else {
            format!("{}\n{}", self.head, self.items.join("\n"))
        })
    }
}

fn render(sections: &[Section]) -> String {
    let parts: Vec<String> = sections.iter().filter_map(Section::render).collect();
    parts.join("\n")
}

/// The repo map of `index` within `budget` tokens, or an empty string for an empty
/// workspace. Sections come in the order languages, directories, entry points, key symbols;
/// over budget, items leave from the last section first, so the first lines survive longest.
/// Everything is sorted with explicit tie-breakers: the same repository gives the same bytes.
///
/// This is synchronous and does file and database reads: the engine runs it off the startup
/// path (a blocking task) once per session. It waits up to 200 ms for the index's first
/// refresh to finish (polling [`Index::is_ready`]; `wait_ready` is async and cannot be
/// awaited here) and otherwise goes ahead with the tree loaded from the database.
pub fn build(index: &Index, est: &TokenEstimator, budget: u32) -> String {
    let start = Instant::now();
    while !index.is_ready() && start.elapsed() < READY_WAIT {
        std::thread::sleep(READY_POLL);
    }
    let tree = index.tree();
    if tree.is_empty() {
        return String::new();
    }

    let root_files = tree.list_dir(Path::new(""));
    let dirs = tree.dirs(DIR_DEPTH);

    let mut sections = vec![
        languages(tree.len(), &root_files, &dirs),
        directories(&dirs),
        entry_points(index),
        key_symbols(index),
    ];
    loop {
        let text = render(&sections);
        if text.is_empty() || est.estimate(&text) <= budget {
            return text;
        }
        match sections.iter_mut().rev().find(|s| !s.items.is_empty()) {
            Some(last) => {
                last.items.pop();
            }
            None => return String::new(),
        }
    }
}

fn languages(total: usize, root_files: &[hx_index::FileEntry], dirs: &[DirInfo]) -> Section {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for lang in root_files.iter().filter_map(|f| f.lang) {
        *counts.entry(lang.as_str()).or_default() += 1;
    }
    // Top-level directories cover every file below the root.
    for dir in dirs.iter().filter(|d| d.path.components().count() == 1) {
        for (lang, n) in &dir.langs {
            *counts.entry(lang.as_str()).or_default() += n;
        }
    }
    let mut counts: Vec<(&str, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    Section {
        head: format!("Languages ({total} files):"),
        inline: true,
        items: counts
            .into_iter()
            .take(MAX_LANGS)
            .map(|(l, n)| format!("{l} {n}"))
            .collect(),
    }
}

fn directories(dirs: &[DirInfo]) -> Section {
    let mut dirs: Vec<&DirInfo> = dirs.iter().collect();
    dirs.sort_by(|a, b| b.files.cmp(&a.files).then(a.path.cmp(&b.path)));
    let items = dirs
        .into_iter()
        .take(MAX_DIRS)
        .map(|d| {
            let langs: Vec<String> = d
                .langs
                .iter()
                .take(MAX_DIR_LANGS)
                .map(|(l, n): &(Lang, usize)| format!("{} {n}", l.as_str()))
                .collect();
            let name = slashed(&d.path);
            if langs.is_empty() {
                format!("{name}/ {}", d.files)
            } else {
                format!("{name}/ {} ({})", d.files, langs.join(", "))
            }
        })
        .collect();
    Section {
        head: "Dirs:".into(),
        inline: false,
        items,
    }
}

fn entry_points(index: &Index) -> Section {
    let tree = index.tree();
    let mut items = Vec::new();
    for name in ["main.rs", "lib.rs"] {
        let mut parents: Vec<PathBuf> = tree
            .glob(name, Path::new(""))
            .into_iter()
            .map(|p| p.parent().map(Path::to_path_buf).unwrap_or_default())
            .collect();
        // Shallow first: the crate root before nested ones.
        parents.sort_by(|a, b| {
            a.components()
                .count()
                .cmp(&b.components().count())
                .then(a.cmp(b))
        });
        parents.dedup();
        if !parents.is_empty() {
            let names: Vec<String> = parents
                .iter()
                .map(|p| {
                    if p.as_os_str().is_empty() {
                        ".".into()
                    } else {
                        slashed(p)
                    }
                })
                .collect();
            items.push(format!("{name}: {}", capped(names, MAX_LIST)));
        }
    }
    if let Some(members) = workspace_members(index) {
        items.push(format!("workspace: {members}"));
    }
    if let Some(scripts) = package_scripts(index) {
        items.push(format!("scripts: {scripts}"));
    }
    Section {
        head: "Entry points:".into(),
        inline: false,
        items,
    }
}

/// `[workspace] members` of the root `Cargo.toml`, patterns expanded against the tree and
/// grouped by parent directory (`crates/{a,b}`).
fn workspace_members(index: &Index) -> Option<String> {
    let text = read_manifest(index, "Cargo.toml")?;
    let manifest: toml::Table = text.parse().ok()?;
    let patterns = manifest.get("workspace")?.get("members")?.as_array()?;
    let tree = index.tree();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for pattern in patterns.iter().filter_map(toml::Value::as_str) {
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        if pattern.is_empty() {
            continue;
        }
        for manifest in tree.glob(&format!("{pattern}/Cargo.toml"), Path::new("")) {
            if let Some(dir) = manifest.parent().filter(|d| !d.as_os_str().is_empty()) {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    if dirs.is_empty() {
        return None;
    }
    let extra = dirs.len().saturating_sub(MAX_MEMBERS);
    dirs.truncate(MAX_MEMBERS);
    // Group by parent in path order; BTreeMap keeps the groups sorted.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for dir in &dirs {
        let parent = dir.parent().map(slashed).unwrap_or_default();
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
        groups.entry(parent).or_default().extend(name);
    }
    let mut parts: Vec<String> = groups
        .into_iter()
        .map(
            |(parent, names)| match (parent.is_empty(), names.as_slice()) {
                (true, _) => names.join(", "),
                (false, [one]) => format!("{parent}/{one}"),
                (false, _) => format!("{parent}/{{{}}}", names.join(",")),
            },
        )
        .collect();
    if extra > 0 {
        parts.push(format!("+{extra} more"));
    }
    Some(parts.join(", "))
}

/// The keys of the root `package.json` `scripts` object, sorted.
fn package_scripts(index: &Index) -> Option<String> {
    let text = read_manifest(index, "package.json")?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut names: Vec<String> = value.get("scripts")?.as_object()?.keys().cloned().collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    Some(capped(names, MAX_SCRIPTS))
}

fn key_symbols(index: &Index) -> Section {
    Section {
        head: "Key symbols:".into(),
        inline: true,
        items: index
            .top_referenced(MAX_SYMBOLS)
            .into_iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect(),
    }
}

/// A small root-level manifest, read only if the tree has it (so ignored files stay
/// unread), is a regular file (no symlink out of the workspace) and fits [`MANIFEST_CAP`].
fn read_manifest(index: &Index, name: &str) -> Option<String> {
    index.tree().get(Path::new(name))?;
    let path = index.root().join(name);
    if !std::fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&path)
        .ok()?
        .take(MANIFEST_CAP + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MANIFEST_CAP {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// The first `max` names comma-separated, then how many were left out.
fn capped(mut names: Vec<String>, max: usize) -> String {
    let extra = names.len().saturating_sub(max);
    names.truncate(max);
    let mut out = names.join(", ");
    if extra > 0 {
        out.push_str(&format!(", +{extra} more"));
    }
    out
}

fn slashed(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
