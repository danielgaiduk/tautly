//! Anchor hashing and staleness.

use std::path::Path;

use hx_index::Index;

use crate::MemAnchor;

/// The hash `symbol` of `rel` has now. A qualified symbol (`Type::method`, `Class.method`)
/// is looked up by its parent, so it never lands on a same-named definition elsewhere in the
/// file. The kind is not part of an anchor, so it is not narrowed.
fn symbol_hash(index: &Index, rel: &Path, symbol: &str) -> Option<String> {
    let whole = index.symbol_hash_in(rel, symbol, None, None);
    let qualified = || {
        let (parent, name) = split_qualified(symbol)?;
        index.symbol_hash_in(rel, name, Some(parent), None)
    };
    whole.or_else(qualified).filter(|h| !h.is_empty())
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

/// The hash of the anchored code now, or why there is none: the file is not indexed, is
/// too large to have been hashed, or no such symbol is in it.
pub(crate) fn anchor_hash(
    index: &Index,
    rel: &Path,
    symbol: Option<&str>,
) -> Result<String, String> {
    let file = index
        .file_hash(rel)
        .ok_or("the file is not indexed (missing, ignored or binary)")?;
    if file.is_empty() {
        return Err("the file is too large to index, so its content cannot be hashed".into());
    }
    match symbol {
        None => Ok(file),
        Some(name) => symbol_hash(index, rel, name)
            .ok_or_else(|| format!("symbol `{name}` was not found in the file")),
    }
}

/// Whether the code `a` points at differs from what it was (or is gone).
pub(crate) fn is_stale(index: &Index, a: &MemAnchor) -> bool {
    match anchor_hash(index, &a.path, a.symbol.as_deref()) {
        Ok(now) => now != a.hash,
        Err(_) => true,
    }
}
