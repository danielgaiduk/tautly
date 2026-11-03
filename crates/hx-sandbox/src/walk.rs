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
/// the walk is back on the filesystem and continues to follow whatever links come
/// next. A path whose only `..` are over names that exist is a single `canonicalize`.
///
/// Errors other than "missing" (a file used as a directory, no permission) are returned
/// as they are, unless the path has by then left `root`: what exists outside the
/// workspace is not revealed, the result is `Resolved::Outside` instead.
pub(crate) fn walk(root: &Path, target: &Path) -> std::io::Result<Resolved> {
    let mut cur = if target.is_absolute() {
        PathBuf::from("/")
    } else {
        root.to_path_buf()
    };
    let mut queue: VecDeque<OsString> = components_of(target).collect();
    // Names below `cur` that do not exist; `cur` itself is canonical and exists.
    let mut missing: Vec<OsString> = Vec::new();
    let mut links = 0;
    let finish = |cur: PathBuf, missing: Vec<OsString>| {
        let full = cur.join(missing.iter().collect::<PathBuf>());
        match full.strip_prefix(root) {
            Ok(rel) => Resolved::Inside(rel.to_path_buf()),
            Err(_) => Resolved::Outside(full),
        }
    };
    let io_or_outside = |e: std::io::Error, cur: &Path| {
        if cur.starts_with(root) {
            Err(e)
        } else {
            Ok(Resolved::Outside(cur.to_path_buf()))
        }
    };
    loop {
        if !missing.is_empty() {
            match queue.pop_front() {
                None => return Ok(finish(cur, missing)),
                Some(name) if name == ".." => {
                    missing.pop();
                }
                Some(name) => missing.push(name),
            }
            continue;
        }
        if queue.is_empty() {
            return Ok(finish(cur, missing));
        }
        // The longest run of the remaining names that exists, in one call.
        let mut advanced = false;
        for keep in (1..=queue.len()).rev() {
            let mut prefix = cur.clone();
            prefix.extend(queue.iter().take(keep));
            match prefix.canonicalize() {
                Ok(canon) => {
                    cur = canon;
                    queue.drain(..keep);
                    advanced = true;
                    break;
                }
                Err(e) if is_loop(&e) => return Ok(Resolved::Loop),
                // Missing, a dangling link, or an unusable prefix: try a shorter one.
                Err(_) => {}
            }
        }
        if advanced {
            continue;
        }
        // Nothing more resolves, so the next name is missing, a dangling link, or not
        // reachable (its parent is a file).
        let Some(name) = queue.pop_front() else {
            return Ok(finish(cur, missing));
        };
        if name == ".." {
            // Only after a file used as a directory (`f/..`); stay lexical.
            cur.pop();
            continue;
        }
        let next = cur.join(&name);
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Ok(Resolved::Loop);
                }
                let dest = std::fs::read_link(&next)?;
                if dest.is_absolute() {
                    cur = PathBuf::from("/");
                }
                for part in components_of(&dest).collect::<Vec<_>>().into_iter().rev() {
                    queue.push_front(part);
                }
            }
            Ok(_) => missing.push(name),
            Err(e) if e.kind() == ErrorKind::NotFound => missing.push(name),
            Err(e) => return io_or_outside(e, &cur),
        }
    }
}

/// The names of `p` to walk: `..` and normal names; `.` and the root are implied.
fn components_of(p: &Path) -> impl Iterator<Item = OsString> + '_ {
    p.components().filter_map(|c| match c {
        Component::ParentDir => Some(OsString::from("..")),
        Component::Normal(s) => Some(s.to_owned()),
        _ => None,
    })
}

/// errno ELOOP (`ErrorKind::FilesystemLoop` is not stable yet).
#[cfg(target_os = "macos")]
pub(crate) const ELOOP: i32 = 62;
#[cfg(not(target_os = "macos"))]
pub(crate) const ELOOP: i32 = 40;

fn is_loop(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(ELOOP)
}
