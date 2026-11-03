//! Anchor hashing and staleness.

use std::collections::BTreeSet;
use std::path::Path;

use hx_index::{Index, IndexError};

use crate::MemAnchor;

/// What the index says about an anchor now.
pub(crate) enum Probe {
    /// The anchored code has this hash.
    Hash(String),
    /// There is nothing to hash, and why: the file is not indexed or too large, or the
    /// symbol is not in it.
    Gone(String),
}

/// The hash `symbol` of `rel` has now. A qualified symbol (`Type::method`, `Class.method`)
/// is looked up by its parent, so it never lands on a same-named definition elsewhere in the
/// file. The kind is not part of an anchor, so it is not narrowed.
fn symbol_hash(index: &Index, rel: &Path, symbol: &str) -> Result<Option<String>, IndexError> {
    let mut found = index.try_symbol_hash_in(rel, symbol, None, None)?;
    if found.is_none()
        && let Some((parent, name)) = split_qualified(symbol)
    {
        found = index.try_symbol_hash_in(rel, name, Some(parent), None)?;
    }
    Ok(found.filter(|h| !h.is_empty()))
}

/// `(parent, name)` of `a::Type::name` or `Type.name`: the last two segments.
fn split_qualified(symbol: &str) -> Option<(&str, &str)> {
    let (head, name) = symbol
        .rsplit_once("::")
        .or_else(|| symbol.rsplit_once('.'))?;
    let parent = head.rsplit("::").next().unwrap_or(head);
    let parent = parent.rsplit('.').next().unwrap_or(parent);
    (!parent.is_empty() && !name.is_empty()).then_some((parent, name))
}

/// The hash of the anchored code now, or why there is none. An `Err` is a database
/// error, which says nothing about the code.
pub(crate) fn probe(index: &Index, rel: &Path, symbol: Option<&str>) -> Result<Probe, IndexError> {
    let Some(file) = index.try_file_hash(rel)? else {
        return Ok(Probe::Gone(
            "the file is not indexed (missing, ignored or binary)".into(),
        ));
    };
    if file.is_empty() {
        return Ok(Probe::Gone(
            "the file is too large to index, so its content cannot be hashed".into(),
        ));
    }
    match symbol {
        None => Ok(Probe::Hash(file)),
        Some(name) => Ok(match symbol_hash(index, rel, name)? {
            Some(h) => Probe::Hash(h),
            None => Probe::Gone(format!("symbol `{name}` was not found in the file")),
        }),
    }
}

/// Whether the code `a` points at differs from what it was (or is gone). An `Err` is a
/// database error: the answer is unknown.
pub(crate) fn is_stale(index: &Index, a: &MemAnchor) -> Result<bool, IndexError> {
    Ok(match probe(index, &a.path, a.symbol.as_deref())? {
        Probe::Hash(now) => now != a.hash,
        Probe::Gone(_) => true,
    })
}

/// Why an unqualified `symbol` cannot be anchored: several definitions of the file go by
/// that name (in different types), and only the first would be tracked. None if it is
/// unambiguous.
pub(crate) fn ambiguity(
    index: &Index,
    rel: &Path,
    symbol: &str,
) -> Result<Option<String>, IndexError> {
    let candidates: BTreeSet<String> = index
        .outline(rel)?
        .into_iter()
        .filter(|r| r.name == symbol)
        .map(|r| match r.parent {
            Some(p) => format!("{p}::{}", r.name),
            None => r.name,
        })
        .collect();
    if candidates.len() < 2 {
        return Ok(None);
    }
    let list: Vec<String> = candidates.iter().map(|c| format!("`{c}`")).collect();
    Ok(Some(format!(
        "`{symbol}` is ambiguous in the file; name one of {}",
        list.join(", ")
    )))
}
