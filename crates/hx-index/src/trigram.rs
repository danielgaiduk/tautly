//! Trigram text index: which files can contain a pattern's required literals.
//!
//! Every UTF-8 text file within the size cap has its distinct three-byte windows
//! stored (ASCII-lowercased, so the index is a case-insensitive superset). A pattern
//! is reduced to a conjunction of literal alternatives that every match must contain;
//! the postings of their trigrams give the files that can match. The caller confirms
//! with the real regex, so the answer only has to be a superset of the truth.

use std::collections::BTreeSet;
use std::path::PathBuf;

use regex_syntax::hir::{Class, Hir, HirKind};
use rusqlite::{Connection, params_from_iter};

use crate::store::{Index, IndexError};

/// What a literal must contain to be worth a lookup.
const MIN_LITERAL: usize = 3;
/// Alternatives tracked for one sub-pattern before it is treated as unconstrained.
const MAX_SET: usize = 32;
/// Characters in a class that is expanded into alternatives.
const MAX_CLASS: u32 = 32;
/// Trigrams intersected per literal: a subset only widens the candidates.
const MAX_TRIGRAMS_PER_LITERAL: usize = 16;

/// The files a text search has to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidates {
    /// A superset of the files that can match, sorted by path.
    Files(Vec<PathBuf>),
    /// The index cannot narrow the search: scan everything.
    FullScan,
}

impl Index {
    /// Narrow a regex search to the files that can contain a match.
    ///
    /// Patterns are Rust-regex syntax; put case-insensitivity in the pattern
    /// (`(?i)`). Every file with a match is in the result, but not every file in the
    /// result matches. Files the index cannot vouch for (over 1 MB, not UTF-8) are
    /// always included. A pattern with no literal of at least 3 bytes, a syntax error,
    /// an index that has not finished refreshing, and any database error all yield
    /// [`Candidates::FullScan`].
    pub fn grep_candidates(&self, pattern: &str) -> Candidates {
        if !self.is_ready() {
            return Candidates::FullScan;
        }
        let Some(plan) = plan(pattern) else {
            return Candidates::FullScan;
        };
        match self.inner.with_conn(|conn| lookup(conn, &plan)) {
            Ok(mut files) => {
                files.extend(self.inner.opaque_files());
                files.sort();
                files.dedup();
                Candidates::Files(files.into_iter().map(PathBuf::from).collect())
            }
            Err(_) => Candidates::FullScan,
        }
    }
}

/// The distinct trigrams of `bytes`, ASCII-lowercased, each packed into a `u32`.
pub(crate) fn trigrams(bytes: &[u8]) -> Vec<u32> {
    let mut out: Vec<u32> = bytes
        .windows(3)
        .map(|w| {
            pack(
                w[0].to_ascii_lowercase(),
                w[1].to_ascii_lowercase(),
                w[2].to_ascii_lowercase(),
            )
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn pack(a: u8, b: u8, c: u8) -> u32 {
    (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c)
}

/// Clauses that must all hold; a clause holds for a file that contains every trigram
/// of at least one of its literals.
type Plan = Vec<Vec<Vec<u8>>>;

fn lookup(conn: &Connection, plan: &Plan) -> Result<Vec<String>, IndexError> {
    let mut ids: Option<BTreeSet<i64>> = None;
    for clause in plan {
        let mut any: BTreeSet<i64> = BTreeSet::new();
        for literal in clause {
            any.extend(literal_files(conn, literal)?);
        }
        let next: BTreeSet<i64> = match ids {
            None => any,
            Some(have) => have.intersection(&any).copied().collect(),
        };
        let done = next.is_empty();
        ids = Some(next);
        if done {
            break;
        }
    }
    let mut path = conn.prepare_cached("SELECT path FROM files WHERE rowid = ?1")?;
    let mut out = Vec::new();
    for id in ids.unwrap_or_default() {
        if let Ok(p) = path.query_row([id], |r| r.get::<_, String>(0)) {
            out.push(p);
        }
    }
    Ok(out)
}

/// File ids whose content contains all trigrams of `literal`.
fn literal_files(conn: &Connection, literal: &[u8]) -> Result<Vec<i64>, IndexError> {
    let mut tris = trigrams(literal);
    tris.truncate(MAX_TRIGRAMS_PER_LITERAL);
    let sql = vec!["SELECT file_id FROM trigrams WHERE tri = ?"; tris.len()].join(" INTERSECT ");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params_from_iter(tris.iter().map(|t| i64::from(*t))), |r| {
        r.get::<_, i64>(0)
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The required-literal plan for `pattern`; None when no clause is worth a lookup.
fn plan(pattern: &str) -> Option<Plan> {
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let mut clauses = info(&hir).into_clauses();
    clauses.retain(|c| !c.is_empty() && c.iter().all(|l| l.len() >= MIN_LITERAL));
    clauses.sort();
    clauses.dedup();
    (!clauses.is_empty()).then_some(clauses)
}

/// What a sub-pattern requires of any text it matches (all literals ASCII-lowercased).
#[derive(Default)]
struct Info {
    /// Every match is exactly one of these strings.
    exact: Option<Vec<Vec<u8>>>,
    /// Every match also contains, for each clause, at least one of its literals.
    clauses: Vec<Vec<Vec<u8>>>,
}

impl Info {
    fn exact(set: Vec<Vec<u8>>) -> Info {
        Info {
            exact: Some(set),
            clauses: Vec::new(),
        }
    }

    /// The sub-pattern's requirements as clauses, exact or not.
    fn into_clauses(self) -> Vec<Vec<Vec<u8>>> {
        let mut clauses = self.clauses;
        clauses.extend(self.exact);
        clauses
    }
}

fn info(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::exact(vec![Vec::new()]),
        HirKind::Literal(l) => Info::exact(vec![l.0.to_ascii_lowercase()]),
        HirKind::Class(c) => class_set(c).map_or_else(Info::default, Info::exact),
        HirKind::Capture(c) => info(&c.sub),
        HirKind::Repetition(r) => {
            if r.min == 0 {
                Info::default()
            } else if r.min == 1 && r.max == Some(1) {
                info(&r.sub)
            } else {
                Info {
                    exact: None,
                    clauses: info(&r.sub).into_clauses(),
                }
            }
        }
        HirKind::Concat(parts) => concat(parts),
        HirKind::Alternation(alts) => alternation(alts),
    }
}

fn concat(parts: &[Hir]) -> Info {
    let mut run: Vec<Vec<u8>> = vec![Vec::new()];
    let mut clauses: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut inexact = false;
    for part in parts {
        let part = info(part);
        match part.exact {
            Some(set) => {
                let product = cross(&run, &set);
                if product.len() <= MAX_SET {
                    run = product;
                } else {
                    // Too many combinations: keep the run so far as a requirement and
                    // start a new one (adjacency across the break is dropped).
                    clauses.push(std::mem::replace(&mut run, set));
                    inexact = true;
                }
            }
            None => {
                clauses.push(std::mem::replace(&mut run, vec![Vec::new()]));
                clauses.extend(part.clauses);
                inexact = true;
            }
        }
    }
    if inexact {
        clauses.push(run);
        Info {
            exact: None,
            clauses,
        }
    } else {
        Info::exact(run)
    }
}

fn alternation(alts: &[Hir]) -> Info {
    let infos: Vec<Info> = alts.iter().map(info).collect();
    let mut union: BTreeSet<Vec<u8>> = BTreeSet::new();
    if infos.iter().all(|i| i.exact.is_some()) {
        for i in infos {
            union.extend(i.exact.into_iter().flatten());
        }
        return if union.len() <= MAX_SET {
            Info::exact(union.into_iter().collect())
        } else {
            Info::default()
        };
    }
    // Some alternative is not a fixed string: each one still has to offer a usable
    // requirement, and any match contains one of the offers.
    for i in infos {
        let Some(best) = i
            .into_clauses()
            .into_iter()
            .filter(|c| !c.is_empty() && c.iter().all(|l| l.len() >= MIN_LITERAL))
            .max_by_key(|c| c.iter().map(Vec::len).min())
        else {
            return Info::default();
        };
        union.extend(best);
    }
    if union.len() <= MAX_SET {
        Info {
            exact: None,
            clauses: vec![union.into_iter().collect()],
        }
    } else {
        Info::default()
    }
}

fn cross(a: &[Vec<u8>], b: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
    for x in a {
        for y in b {
            out.insert([x.as_slice(), y.as_slice()].concat());
            if out.len() > MAX_SET {
                return out.into_iter().collect();
            }
        }
    }
    out.into_iter().collect()
}

/// The strings a small class can match, ASCII-lowercased; None for a large class.
fn class_set(class: &Class) -> Option<Vec<Vec<u8>>> {
    let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
    match class {
        Class::Unicode(u) => {
            let size: u32 = u
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1)
                .sum();
            if size > MAX_CLASS {
                return None;
            }
            for range in u.iter() {
                for c in range.start()..=range.end() {
                    let mut buf = [0u8; 4];
                    out.insert(c.encode_utf8(&mut buf).as_bytes().to_ascii_lowercase());
                }
            }
        }
        Class::Bytes(b) => {
            let size: u32 = b
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1)
                .sum();
            if size > MAX_CLASS {
                return None;
            }
            for range in b.iter() {
                for byte in range.start()..=range.end() {
                    out.insert(vec![byte.to_ascii_lowercase()]);
                }
            }
        }
    }
    Some(out.into_iter().collect())
}
