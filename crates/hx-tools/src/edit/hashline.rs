//! `hashline`: edit by line anchors (`<n>:<hash>`) as printed by `read`.

/// Hex digits of the line hash `read` prints.
const HASH_LEN: usize = 4;

/// A line reference: 1-based line number plus the first 4 hex of the blake3 of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub line: usize,
    pub hash: String,
}

impl Anchor {
    /// Parse `<n>:<hash>` (what `read` prints before the `|`).
    pub fn parse(s: &str) -> Result<Self, HashlineError> {
        let bad = || HashlineError::BadAnchor(s.to_string());
        let (line, hash) = s.trim().split_once(':').ok_or_else(bad)?;
        if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        if hash.len() != HASH_LEN || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        Ok(Self {
            line: line.parse().map_err(|_| bad())?,
            hash: hash.to_ascii_lowercase(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashlineOp {
    Replace {
        start: Anchor,
        end: Anchor,
        lines: Vec<String>,
    },
    InsertAfter {
        at: Anchor,
        lines: Vec<String>,
    },
    Delete {
        start: Anchor,
        end: Anchor,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashlineError {
    #[error("invalid anchor `{0}`; expected `<line>:<4-hex hash>` as printed by `read`")]
    BadAnchor(String),
    #[error("line {line} changed since it was read (anchor hash mismatch)")]
    Stale { line: usize },
    #[error("line {line} is past the end of the file ({total} lines)")]
    OutOfRange { line: usize, total: usize },
    #[error("range start line {start} is after end line {end}")]
    BadRange { start: usize, end: usize },
    #[error("operations at lines {a} and {b} overlap")]
    Overlap { a: usize, b: usize },
}

/// The hash `read` prints for a line.
fn line_hash(line: &str) -> String {
    blake3::hash(line.as_bytes()).to_hex()[..HASH_LEN].to_string()
}

/// Check `anchor` against the original lines. `read` hashes the first line of a BOM file
/// with its BOM, which the BOM-less text given to [`apply`] no longer has.
fn check(lines: &[&str], anchor: &Anchor) -> Result<(), HashlineError> {
    let total = lines.len();
    if anchor.line == 0 || anchor.line > total {
        return Err(HashlineError::OutOfRange {
            line: anchor.line,
            total,
        });
    }
    let line = lines[anchor.line - 1];
    let fresh = line_hash(line) == anchor.hash
        || (anchor.line == 1 && line_hash(&format!("\u{FEFF}{line}")) == anchor.hash);
    if fresh {
        Ok(())
    } else {
        Err(HashlineError::Stale { line: anchor.line })
    }
}

/// One validated edit: replace original lines `[from, to)` (0-based) with `lines`.
struct Edit {
    from: usize,
    to: usize,
    insert: bool,
    lines: Vec<String>,
    /// First anchored line (1-based), for error messages.
    line: usize,
}

impl Edit {
    /// The 1-based line interval a range edit occupies.
    fn span(&self) -> (usize, usize) {
        (self.from + 1, self.to)
    }

    fn conflicts(&self, other: &Edit) -> bool {
        match (self.insert, other.insert) {
            (false, false) => {
                let (s1, e1) = self.span();
                let (s2, e2) = other.span();
                s1 <= e2 && s2 <= e1
            }
            (true, true) => self.from == other.from,
            // Inserting after the line just before a range, or after its last line, is
            // fine; after any line in between would land inside the replaced block.
            (true, false) => other.span().0 <= self.from && self.from < other.span().1,
            (false, true) => other.conflicts(self),
        }
    }
}

fn split_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .flat_map(|l| l.split('\n'))
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect()
}

fn range_edit(
    lines: &[&str],
    start: &Anchor,
    end: &Anchor,
    replacement: &[String],
) -> Result<Edit, HashlineError> {
    check(lines, start)?;
    check(lines, end)?;
    if start.line > end.line {
        return Err(HashlineError::BadRange {
            start: start.line,
            end: end.line,
        });
    }
    Ok(Edit {
        from: start.line - 1,
        to: end.line,
        insert: false,
        lines: split_lines(replacement),
        line: start.line,
    })
}

/// Apply `ops` to `text` (LF line endings). Every anchor is validated against the
/// original text; ops are applied bottom-up so line numbers stay valid. A stale anchor is
/// never repaired by searching for the line elsewhere.
pub fn apply(text: &str, ops: &[HashlineOp]) -> Result<String, HashlineError> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let original: Vec<&str> = text.lines().collect();

    let mut edits = Vec::with_capacity(ops.len());
    for op in ops {
        edits.push(match op {
            HashlineOp::Replace {
                start,
                end,
                lines: new,
            } => range_edit(&original, start, end, new)?,
            HashlineOp::Delete { start, end } => range_edit(&original, start, end, &[])?,
            HashlineOp::InsertAfter { at, lines: new } => {
                check(&original, at)?;
                Edit {
                    from: at.line,
                    to: at.line,
                    insert: true,
                    lines: split_lines(new),
                    line: at.line,
                }
            }
        });
    }
    for (i, a) in edits.iter().enumerate() {
        if let Some(b) = edits[i + 1..].iter().find(|b| a.conflicts(b)) {
            return Err(HashlineError::Overlap {
                a: a.line.min(b.line),
                b: a.line.max(b.line),
            });
        }
    }

    // Bottom-up; at the same position a range edit goes first so an insertion after the
    // line above it ends up in front of the replacement.
    edits.sort_by(|a, b| b.from.cmp(&a.from).then(a.insert.cmp(&b.insert)));
    for e in edits {
        lines.splice(e.from..e.to, e.lines);
    }
    let mut out = lines.join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    Ok(out)
}
