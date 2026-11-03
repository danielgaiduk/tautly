//! The one path resolver behind both `LocalSandbox::resolve` and the permission
//! policy's symlink check, so the file the policy approves is the file a write reaches.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use crate::protected::Resolved;

/// Symlinks followed before giving up (the kernel's own limit is similar).
const MAX_LINKS: usize = 40;

/// Resolve `target` (relative to the canonical directory `root`, or absolute) the way
/// the kernel would: every symlink along it is followed (dangling ones too: a write
/// through one creates its target) and `..` applies to the path resolved so far, never
/// to a name as written.
///
/// A missing component is the one place this is lenient: the kernel fails
/// `missing/../x` with ENOENT, but a tool may create `missing` first, so the walk
/// treats the pair as cancelling. Once a `..` pops back onto a component that exists,
/// the walk is back on the filesystem and keeps following whatever links come next.
///
/// Cost: a path that exists is a single `canonicalize` (which also yields the on-disk
/// spelling). Otherwise the walk goes one name at a time, one `lstat` per name (plus
/// one `readlink` per link) and a final `canonicalize` of the part that exists;
/// `..` over a name that exists is a pop of the symlink-free path walked so far, never
/// a re-scan, so the work is linear in the length of `target` however many `..` it has.
///
/// Errors other than "missing" (a file used as a directory, no permission) are returned
/// as they are, unless the path has by then left `root`: what exists outside the
/// workspace is not revealed, the result is `Resolved::Outside` instead. Likewise a
/// symlink loop reached outside the root.
pub(crate) fn walk(root: &Path, target: &Path) -> std::io::Result<Resolved> {
    let (mut cur, full) = if target.is_absolute() {
        (PathBuf::from("/"), target.to_path_buf())
    } else {
        (root.to_path_buf(), root.join(target))
    };
    if let Ok(canon) = full.canonicalize() {
        return Ok(classify(root, canon, &[]));
    }
    // `cur` stays a real directory path with no symlink in it (its case may be the
    // one the caller typed; the canonicalize at the end puts on-disk spelling back).
    let mut queue: VecDeque<OsString> = components_of(target).collect();
    // Names below `cur` that do not exist.
    let mut missing: Vec<OsString> = Vec::new();
    let mut links = 0;
    let io_or_outside = |e: std::io::Error, cur: &Path| {
        if cur.starts_with(root) {
            Err(e)
        } else {
            Ok(Resolved::Outside(cur.to_path_buf()))
        }
    };
    while let Some(name) = queue.pop_front() {
        if !missing.is_empty() {
            if name == ".." {
                missing.pop();
            } else {
                missing.push(name);
            }
            continue;
        }
        if name == ".." {
            cur.pop();
            continue;
        }
        let next = cur.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Ok(if cur.starts_with(root) {
                        Resolved::Loop
                    } else {
                        Resolved::Outside(cur)
                    });
                }
                let dest = match std::fs::read_link(&next) {
                    Ok(d) => d,
                    Err(e) => return io_or_outside(e, &cur),
                };
                if dest.is_absolute() {
                    cur = PathBuf::from("/");
                }
                for part in components_of(&dest).rev() {
                    queue.push_front(part);
                }
            }
            Ok(_) => cur = next,
            Err(e) if e.kind() == ErrorKind::NotFound => missing.push(name),
            Err(e) => return io_or_outside(e, &cur),
        }
    }
    if let Ok(canon) = cur.canonicalize() {
        cur = canon;
    }
    Ok(classify(root, cur, &missing))
}

fn classify(root: &Path, existing: PathBuf, missing: &[OsString]) -> Resolved {
    let full = existing.join(missing.iter().collect::<PathBuf>());
    match full.strip_prefix(root) {
        Ok(rel) => Resolved::Inside(rel.to_path_buf()),
        Err(_) => Resolved::Outside(full),
    }
}

/// The names of `p` to walk: `..` and normal names; `.` and the root are implied.
fn components_of(p: &Path) -> impl DoubleEndedIterator<Item = OsString> + '_ {
    p.components().filter_map(|c| match c {
        Component::ParentDir => Some(OsString::from("..")),
        Component::Normal(s) => Some(s.to_owned()),
        _ => None,
    })
}
