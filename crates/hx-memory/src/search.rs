//! BM25 ranking over entry and episode text.

use std::collections::{HashMap, HashSet};

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// Where a recall hit came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitSource {
    /// A project entry, by id.
    Project(String),
    /// An episode, by session id.
    Episode(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub source: HitSource,
    pub title: String,
    pub text: String,
    pub stale: bool,
    pub score: f32,
}

/// Term frequencies of one document (title and body).
#[derive(Debug, Clone, Default)]
pub(crate) struct Doc {
    pub tf: HashMap<String, u32>,
    pub len: u32,
}

/// Lowercase alphanumeric tokens of `text`.
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

pub(crate) fn doc_of(title: &str, body: &str) -> Doc {
    let mut doc = Doc::default();
    for t in tokens(title).chain(tokens(body)) {
        *doc.tf.entry(t).or_insert(0) += 1;
        doc.len += 1;
    }
    doc
}

/// The distinct tokens of a query, in order.
pub(crate) fn query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokens(query).filter(|t| seen.insert(t.clone())).collect()
}

/// Documents matching at least one of `terms`, best first (ties: earlier document first),
/// with their BM25 score. The statistics cover all of `docs`.
pub(crate) fn rank(docs: &[&Doc], terms: &[String]) -> Vec<(usize, f32)> {
    if docs.is_empty() || terms.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as f32;
    let avgdl = (docs.iter().map(|d| d.len as f32).sum::<f32>() / n).max(1.0);
    let idf: Vec<f32> = terms
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|d| d.tf.contains_key(t)).count() as f32;
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();
    let mut scored: Vec<(usize, f32)> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let norm = K1 * (1.0 - B + B * d.len as f32 / avgdl);
            let score: f32 = terms
                .iter()
                .zip(&idf)
                .filter_map(|(t, idf)| {
                    let tf = *d.tf.get(t)? as f32;
                    Some(idf * tf * (K1 + 1.0) / (tf + norm))
                })
                .sum();
            (score > 0.0).then_some((i, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
}

/// The longest prefix of `text` that, followed by `…`, fits in `max_tokens`; None if even
/// the marker alone does not fit. Returns `text` itself when it fits whole.
pub(crate) fn truncate_to(
    text: &str,
    max_tokens: u32,
    est: &dyn Fn(&str) -> u32,
) -> Option<String> {
    if est(text) <= max_tokens {
        return Some(text.to_string());
    }
    let fits = |end: usize| est(&format!("{}…", &text[..end])) <= max_tokens;
    let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    // Largest boundary index whose prefix fits (fits is monotonic in the prefix length).
    let (mut lo, mut hi) = (0, boundaries.len());
    if !fits(0) {
        return None;
    }
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        if fits(boundaries[mid]) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(format!("{}…", text[..boundaries[lo]].trim_end()))
}
