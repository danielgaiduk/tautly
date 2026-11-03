//! Episodic memory: private per-session summaries, read lazily on the first search.

use std::fs;
use std::path::Path;

use crate::entry::{sanitize_title, valid_session_id};
use crate::search::{Doc, doc_of};
use crate::store::read_nofollow;

/// Episodes larger than this are read only up to it.
const MAX_EPISODE_BYTES: u64 = 32 * 1024;
/// Only the newest episodes (by session id, which sorts by time) are searched.
const MAX_EPISODES: usize = 200;

pub(crate) struct Episode {
    pub session_id: String,
    pub title: String,
    pub text: String,
    pub doc: Doc,
}

/// The newest readable episodes in `dir` (already verified not to be a link), by session
/// id. Symlinked files are skipped.
pub(crate) fn load_all(dir: &Path) -> Vec<Episode> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".md")?.to_string();
            valid_session_id(&id).then_some(id)
        })
        .collect();
    ids.sort();
    let skip = ids.len().saturating_sub(MAX_EPISODES);
    ids.drain(..skip);
    ids.into_iter()
        .filter_map(|id| {
            let text =
                read_nofollow(&dir.join(format!("{id}.md")), MAX_EPISODE_BYTES, true).ok()?;
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
        .collect()
}
