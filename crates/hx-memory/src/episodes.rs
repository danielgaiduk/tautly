//! Episodic memory: private per-session summaries, read lazily on the first search.

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::entry::{sanitize_title, valid_session_id};
use crate::search::{Doc, doc_of};

/// Episodes larger than this are read only up to it.
const MAX_EPISODE_BYTES: u64 = 64 * 1024;

pub(crate) struct Episode {
    pub session_id: String,
    pub title: String,
    pub text: String,
    pub doc: Doc,
}

/// Every readable episode in `dir`, by session id. A missing directory has none.
pub(crate) fn load_all(dir: &Path) -> Vec<Episode> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Episode> = read
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let id = path.file_name()?.to_str()?.strip_suffix(".md")?.to_string();
            if !valid_session_id(&id) || !fs::symlink_metadata(&path).ok()?.is_file() {
                return None;
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .ok()?
                .take(MAX_EPISODE_BYTES)
                .read_to_end(&mut bytes)
                .ok()?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let heading = text
                .lines()
                .map(|l| sanitize_title(l.trim_start_matches('#')))
                .find(|l| !l.is_empty());
            let title = heading.unwrap_or_else(|| format!("episode {id}"));
            let doc = doc_of("", &text);
            Some(Episode {
                session_id: id,
                title,
                text,
                doc,
            })
        })
        .collect();
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    out
}
