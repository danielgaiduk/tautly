//! The repo map: a few hundred tokens that tell the model the project's shape (languages,
//! directories, entry points, most-referenced symbols) so it needs no exploration steps to
//! get oriented. It lives in the cache-stable prefix, so it is built once per session
//! ([`crate::Context::init_repo_map`]) and never again.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use hx_index::{DirInfo, Index, Lang};

use crate::sanitize;
use crate::tokens::TokenEstimator;

/// Token budget of the rendered `<repo_map>` block, wrapper included ([`crate::Context::init_repo_map`]
/// takes the wrapper off before calling [`build`]).
pub const BUDGET: u32 = 400;

/// How long [`build`] waits for the index's first refresh before using the tree as loaded
/// from the database.
const READY_WAIT: Duration = Duration::from_millis(200);
const READY_POLL: Duration = Duration::from_millis(10);
/// Directory levels listed below the root.
const DIR_DEPTH: usize = 2;
const MAX_LANGS: usize = 6;
/// Top-level directories listed (largest first); the budget cuts further.
const MAX_TOP_DIRS: usize = 64;
/// Nested directories listed after all top-level ones.
const MAX_NESTED_DIRS: usize = 16;
/// Workspace `members` / `exclude` patterns considered; a manifest can hold thousands and
/// each is compiled, so the rest are ignored.
const MAX_PATTERNS: usize = 256;
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

/// Every top-level directory first (most files first, then name), then nested ones as a
/// lower-priority tail, which the budget drops before any top-level line.
fn directories(dirs: &[DirInfo]) -> Section {
    let mut ranked: Vec<(&DirInfo, String)> = dirs
        .iter()
        .filter_map(|d| Some((d, clean_path(&d.path)?)))
        .collect();
    ranked.sort_by(|a, b| b.0.files.cmp(&a.0.files).then(a.1.cmp(&b.1)));
    let (top, nested): (Vec<_>, Vec<_>) = ranked
        .into_iter()
        .partition(|(d, _)| d.path.components().count() == 1);
    let items = top
        .into_iter()
        .take(MAX_TOP_DIRS)
        .chain(nested.into_iter().take(MAX_NESTED_DIRS))
        .map(|(d, name)| {
            let langs: Vec<String> = d
                .langs
                .iter()
                .take(MAX_DIR_LANGS)
                .map(|(l, n): &(Lang, usize)| format!("{} {n}", l.as_str()))
                .collect();
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
        let names: Vec<String> = parents
            .iter()
            .filter_map(|p| {
                if p.as_os_str().is_empty() {
                    Some(".".to_string())
                } else {
                    clean_path(p)
                }
            })
            .collect();
        if !names.is_empty() {
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

/// `[workspace] members` (minus `exclude`) of the root `Cargo.toml`, matched in one pass
/// against the directories that hold a `Cargo.toml` in the tree, and grouped by parent
/// directory (`crates/{a,b}`). At most [`MAX_PATTERNS`] patterns of each list are used.
fn workspace_members(index: &Index) -> Option<String> {
    let text = read_manifest(index, "Cargo.toml")?;
    let manifest: toml::Table = text.parse().ok()?;
    let workspace = manifest.get("workspace")?;
    let members = pattern_set(workspace.get("members"))?;
    let exclude = pattern_set(workspace.get("exclude"));

    let mut dirs: Vec<PathBuf> = index
        .tree()
        .glob("Cargo.toml", Path::new(""))
        .into_iter()
        .filter_map(|m| m.parent().map(Path::to_path_buf))
        .filter(|d| !d.as_os_str().is_empty())
        .filter(|d| members.is_match(d) && !exclude.as_ref().is_some_and(|e| e.is_match(d)))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs.retain(|d| clean_path(d).is_some());
    if dirs.is_empty() {
        return None;
    }
    let extra = dirs.len().saturating_sub(MAX_MEMBERS);
    // Group by parent in path order; BTreeMap keeps the groups sorted.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for dir in dirs.iter().take(MAX_MEMBERS) {
        let parent = dir
            .parent()
            .map(|p| clean_path(p).unwrap_or_default())
            .unwrap_or_default();
        let name = dir
            .file_name()
            .and_then(|n| sanitize::clean(&n.to_string_lossy()));
        groups.entry(parent).or_default().extend(name);
    }
    let mut parts: Vec<String> = groups
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
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

/// The first [`MAX_PATTERNS`] strings of a TOML array as one path matcher (`*` does not
/// cross `/`); invalid patterns are skipped. None when there is no array or no pattern.
fn pattern_set(value: Option<&toml::Value>) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for pattern in value?
        .as_array()?
        .iter()
        .filter_map(toml::Value::as_str)
        .take(MAX_PATTERNS)
    {
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        if let Ok(glob) = GlobBuilder::new(pattern).literal_separator(true).build() {
            builder.add(glob);
            any = true;
        }
    }
    any.then(|| builder.build().ok()).flatten()
}

/// The keys of the root `package.json` `scripts` object, sorted.
fn package_scripts(index: &Index) -> Option<String> {
    let text = read_manifest(index, "package.json")?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut names: Vec<String> = value.get("scripts")?.as_object()?.keys().cloned().collect();
    names.sort();
    let names: Vec<String> = names.iter().filter_map(|n| sanitize::clean(n)).collect();
    if names.is_empty() {
        return None;
    }
    Some(capped(names, MAX_SCRIPTS))
}

fn key_symbols(index: &Index) -> Section {
    Section {
        head: "Key symbols:".into(),
        inline: true,
        items: index
            .top_referenced(MAX_SYMBOLS)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, n)| Some(format!("{} {n}", sanitize::clean(&name)?)))
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

/// A root-relative path as `a/b`, or None if any component is unsafe to show.
fn clean_path(p: &Path) -> Option<String> {
    let parts: Option<Vec<String>> = p
        .components()
        .map(|c| sanitize::clean(&c.as_os_str().to_string_lossy()))
        .collect();
    parts.map(|parts| parts.join("/"))
}
