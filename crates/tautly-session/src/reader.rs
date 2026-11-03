use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::de::DeserializeOwned;

use crate::blobs::{BlobRef, BlobStore};
use crate::paths;
use crate::record::{Header, Parsed, SCHEMA_VERSION, parse_line};
use crate::{Error, Result};

/// Reads one session log.
///
/// A final line that lacks its trailing newline or is not structurally valid
/// (not a JSON object of a known shape) is the remnant of a crash mid-write: it is skipped and reported by
/// [`truncated_tail`](Self::truncated_tail). A malformed line anywhere else is
/// corruption, as is any line whose payload does not deserialize as `R`; both
/// yield an error from [`records`](Self::records).
pub struct SessionReader<R: DeserializeOwned> {
    path: PathBuf,
    header: Header,
    /// Byte offset just past the header line.
    body_start: u64,
    /// Byte offset of the truncated final line, if there is one.
    tail_start: Option<u64>,
    blobs: BlobStore,
    _record: PhantomData<fn() -> R>,
}

impl<R: DeserializeOwned> SessionReader<R> {
    pub fn open(path: &Path) -> Result<Self> {
        let bad = |reason: &str| Error::BadHeader {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        };
        let mut file = BufReader::new(File::open(path)?);
        let mut first = Vec::new();
        file.read_until(b'\n', &mut first)?;
        if first.last() != Some(&b'\n') {
            return Err(bad("header line is missing or incomplete"));
        }
        let header = match parse_line(&first) {
            Ok(Parsed::Header(h)) => h,
            Ok(_) => return Err(bad("first line is not a header")),
            Err(e) => return Err(bad(&e.to_string())),
        };
        if header.schema_version > SCHEMA_VERSION {
            return Err(Error::UnsupportedSchema {
                path: path.to_path_buf(),
                found: header.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        let body_start = first.len() as u64;
        let tail_start = find_truncated_tail(file.into_inner(), body_start)?;
        let project_dir = path.parent().unwrap_or(Path::new("."));
        Ok(Self {
            path: path.to_path_buf(),
            header,
            body_start,
            tail_start,
            blobs: BlobStore::new(project_dir),
            _record: PhantomData,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Whether the log ended in a partial line that was skipped.
    pub fn truncated_tail(&self) -> bool {
        self.tail_start.is_some()
    }

    pub(crate) fn tail_start(&self) -> Option<u64> {
        self.tail_start
    }

    /// Stream `(offset, record)` pairs in log order. After the first error the
    /// iterator ends.
    pub fn records(&self) -> impl Iterator<Item = Result<(u64, R)>> + use<R> {
        Records::<R> {
            path: self.path.clone(),
            input: Some(Input::Pending(self.body_start)),
            pos: self.body_start,
            tail_start: self.tail_start,
            line: 1,
            next_offset: 0,
            _record: PhantomData,
        }
    }

    pub fn read_blob(&self, r: &BlobRef) -> Result<Vec<u8>> {
        self.blobs.get(r)
    }
}

/// Locate the final line when it is a crash remnant: no trailing newline, or
/// not parseable as a line.
fn find_truncated_tail(mut file: File, body_start: u64) -> Result<Option<u64>> {
    let len = file.metadata()?.len();
    if len <= body_start {
        return Ok(None);
    }
    // Start of the last line = byte after the previous newline.
    let mut last = [0u8; 1];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut last)?;
    let ends_with_newline = last[0] == b'\n';
    let search_end = if ends_with_newline { len - 1 } else { len };
    let start = last_line_start(&mut file, body_start, search_end)?;
    if !ends_with_newline {
        return Ok(Some(start));
    }
    let mut buf = vec![0u8; (len - 1 - start) as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf)?;
    // Structural only: a complete line whose payload is not an `R` is
    // corruption, reported by `records()`, never silently dropped.
    Ok(match parse_line(&buf) {
        Ok(_) => None,
        Err(_) => Some(start),
    })
}

/// Offset of the first byte after the last `\n` in `[floor, end)`, or `floor`.
fn last_line_start(file: &mut File, floor: u64, end: u64) -> Result<u64> {
    const CHUNK: u64 = 8192;
    let mut hi = end;
    let mut buf = vec![0u8; CHUNK as usize];
    while hi > floor {
        let lo = hi.saturating_sub(CHUNK).max(floor);
        let n = (hi - lo) as usize;
        file.seek(SeekFrom::Start(lo))?;
        file.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(lo + i as u64 + 1);
        }
        hi = lo;
    }
    Ok(floor)
}

enum Input {
    Pending(u64),
    Open(BufReader<File>),
}

struct Records<R> {
    path: PathBuf,
    /// `None` once finished (end of log or after an error).
    input: Option<Input>,
    pos: u64,
    tail_start: Option<u64>,
    /// 1-based number of the last line read (header = 1).
    line: u64,
    next_offset: u64,
    _record: PhantomData<fn() -> R>,
}

impl<R: DeserializeOwned> Records<R> {
    fn corrupt(&mut self, reason: String) -> Option<Result<(u64, R)>> {
        self.input = None;
        Some(Err(Error::Corrupt {
            path: self.path.clone(),
            line: self.line,
            reason,
        }))
    }
}

impl<R: DeserializeOwned> Iterator for Records<R> {
    type Item = Result<(u64, R)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.tail_start == Some(self.pos) {
                self.input = None;
            }
            let input = self.input.take()?;
            let mut rd = match input {
                Input::Open(rd) => rd,
                Input::Pending(at) => {
                    let opened = File::open(&self.path).and_then(|mut f| {
                        f.seek(SeekFrom::Start(at))?;
                        Ok(BufReader::new(f))
                    });
                    match opened {
                        Ok(rd) => rd,
                        Err(e) => return Some(Err(e.into())),
                    }
                }
            };
            let mut buf = Vec::new();
            let n = match rd.read_until(b'\n', &mut buf) {
                Ok(n) => n,
                Err(e) => return Some(Err(e.into())),
            };
            if n == 0 {
                return None;
            }
            self.input = Some(Input::Open(rd));
            self.pos += n as u64;
            self.line += 1;
            if buf.last() != Some(&b'\n') {
                return self.corrupt("line has no trailing newline".into());
            }
            match parse_line(&buf) {
                Ok(Parsed::Record { offset, data }) => {
                    if offset != self.next_offset {
                        let reason =
                            format!("expected offset {}, found {offset}", self.next_offset);
                        return self.corrupt(reason);
                    }
                    let data = match serde_json::from_str::<R>(data.get()) {
                        Ok(d) => d,
                        Err(e) => {
                            return self.corrupt(format!("record does not match its type: {e}"));
                        }
                    };
                    self.next_offset += 1;
                    return Some(Ok((offset, data)));
                }
                Ok(Parsed::Unknown) => continue,
                Ok(Parsed::Header(_)) => return self.corrupt("unexpected second header".into()),
                Err(e) => return self.corrupt(e.to_string()),
            }
        }
    }
}

/// One session in the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub created: SystemTime,
    pub path: PathBuf,
    pub last_modified: SystemTime,
}

/// Sessions of the project at `root` under [`paths::data_home`], newest first.
pub fn list(root: &Path) -> Result<Vec<SessionSummary>> {
    list_in(&paths::data_home()?, root)
}

/// Like [`list`] with an explicit data home. Unreadable or partial files are
/// skipped; a project with no sessions yields an empty list.
pub fn list_in(data_home: &Path, root: &Path) -> Result<Vec<SessionSummary>> {
    let dir = paths::project_dir(data_home, root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(reader) = SessionReader::<serde::de::IgnoredAny>::open(&path) else {
            continue;
        };
        let Ok(last_modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let header = reader.header();
        out.push(SessionSummary {
            id: header.session_id.clone(),
            created: header.created,
            path,
            last_modified,
        });
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
    Ok(out)
}
