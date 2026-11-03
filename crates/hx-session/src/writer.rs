use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::marker::PhantomData;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;
use ulid::Ulid;

use crate::blobs::{BlobRef, BlobStore};
use crate::paths;
use crate::reader::SessionReader;
use crate::record::{Header, LineRef, SCHEMA_VERSION};
use crate::{Error, Result};

/// Destination of the log bytes: a file in production, a fault-injecting
/// writer in tests.
trait Sink: Write + Send {
    fn sync(&mut self) -> io::Result<()>;
}

impl Sink for File {
    fn sync(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}

/// Take an exclusive, non-blocking advisory lock held until `file` is dropped.
fn lock_exclusive(file: &File, path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain syscall on a file descriptor owned by `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Err(Error::AlreadyOpen(path.to_path_buf()))
    } else {
        Err(err.into())
    }
}

/// Appends records to one session log.
///
/// At most one writer can have a session open: `create` and `reopen` hold an
/// exclusive advisory lock on the log file until the writer is dropped, and a
/// second `reopen` fails with [`Error::AlreadyOpen`].
///
/// `append` only buffers; call [`checkpoint`](Self::checkpoint) at turn and
/// tool boundaries to flush and fsync. Dropping the writer flushes the buffer
/// (without fsync).
///
/// Any I/O error from `append`, `checkpoint` or `blob` may leave a partial
/// line behind, so it poisons the writer: every later call returns
/// [`Error::Poisoned`] and the caller must [`reopen`](Self::reopen) the
/// session, which truncates the torn tail.
pub struct SessionWriter<R: Serialize> {
    out: BufWriter<Box<dyn Sink>>,
    header: Header,
    path: PathBuf,
    next_offset: u64,
    blobs: BlobStore,
    poisoned: AtomicBool,
    _record: PhantomData<fn(&R)>,
}

impl<R: Serialize> SessionWriter<R> {
    /// Create a new session for the project at `root` under [`paths::data_home`].
    /// `parent` marks a fork: (parent session id, parent record offset).
    pub fn create(root: &Path, parent: Option<(String, u64)>) -> Result<Self> {
        Self::create_in(&paths::data_home(), root, parent)
    }

    /// Like [`create`](Self::create) with an explicit data home.
    pub fn create_in(data_home: &Path, root: &Path, parent: Option<(String, u64)>) -> Result<Self> {
        let dir = paths::project_dir(data_home, root);
        paths::create_private_dir_all(&dir)?;
        let ulid = Ulid::new();
        let header = Header {
            schema_version: SCHEMA_VERSION,
            session_id: ulid.to_string(),
            created: ulid.datetime(),
            root: paths::canonical_root(root),
            parent,
        };
        let path = dir.join(format!("{}.jsonl", header.session_id));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        lock_exclusive(&file, &path)?;
        let mut w = Self::from_file(file, header, path, 0, BlobStore::new(&dir));
        let line = encode(&LineRef::<R>::Header(&w.header))?;
        w.write_line(&line)?;
        w.checkpoint()?;
        Ok(w)
    }

    fn from_file(
        file: File,
        header: Header,
        path: PathBuf,
        next_offset: u64,
        blobs: BlobStore,
    ) -> Self {
        Self {
            out: BufWriter::new(Box::new(file)),
            header,
            path,
            next_offset,
            blobs,
            poisoned: AtomicBool::new(false),
            _record: PhantomData,
        }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn session_id(&self) -> &str {
        &self.header.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_usable(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Relaxed) {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Record an I/O failure: the log may now end in a partial line.
    fn poison(&self, e: io::Error) -> Error {
        self.poisoned.store(true, Ordering::Relaxed);
        e.into()
    }

    fn write_line(&mut self, line: &[u8]) -> Result<()> {
        self.out.write_all(line).map_err(|e| self.poison(e))
    }

    /// Buffer one record; returns its 0-based offset.
    pub fn append(&mut self, rec: &R) -> Result<u64> {
        self.check_usable()?;
        let offset = self.next_offset;
        let line = encode(&LineRef::Record { offset, data: rec })?;
        self.write_line(&line)?;
        self.next_offset += 1;
        Ok(offset)
    }

    /// Flush buffered lines and fsync the log.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.check_usable()?;
        let synced = self.out.flush().and_then(|()| self.out.get_mut().sync());
        synced.map_err(|e| self.poison(e))
    }

    /// Store `bytes` content-addressed; identical bytes are stored once.
    pub fn blob(&self, bytes: &[u8]) -> Result<BlobRef> {
        self.check_usable()?;
        match self.blobs.put(bytes) {
            Err(Error::Io(e)) => Err(self.poison(e)),
            other => other,
        }
    }
}

impl<R: Serialize + DeserializeOwned> SessionWriter<R> {
    /// Reopen an existing session log for appending (resume). Takes the
    /// writer lock first ([`Error::AlreadyOpen`] if another writer holds it),
    /// then validates the header and every record (a malformed line other than
    /// a torn final one, or a payload that is not an `R`, is
    /// [`Error::Corrupt`] and nothing is modified), truncates a torn final
    /// line, and continues offsets after the last good record.
    pub fn reopen(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().append(true).open(path)?;
        lock_exclusive(&file, path)?;
        let reader = SessionReader::<R>::open(path)?;
        let mut count = 0;
        for rec in reader.records() {
            rec?;
            count += 1;
        }
        if let Some(tail_start) = reader.tail_start() {
            file.set_len(tail_start)?;
            file.sync_data()?;
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        Ok(Self::from_file(
            file,
            reader.header().clone(),
            path.to_path_buf(),
            count,
            BlobStore::new(dir),
        ))
    }
}

fn encode<R: Serialize>(line: &LineRef<'_, R>) -> Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(line)?;
    buf.push(b'\n');
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts `budget` bytes, then fails every write; `sync` fails on demand.
    struct Failing {
        budget: usize,
        sync_fails: bool,
    }

    impl Write for Failing {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.budget == 0 {
                return Err(io::Error::other("disk full"));
            }
            let n = buf.len().min(self.budget);
            self.budget -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Sink for Failing {
        fn sync(&mut self) -> io::Result<()> {
            if self.sync_fails {
                Err(io::Error::other("eio"))
            } else {
                Ok(())
            }
        }
    }

    fn writer(sink: Failing, blob_dir: &Path) -> SessionWriter<String> {
        SessionWriter {
            out: BufWriter::new(Box::new(sink)),
            header: Header {
                schema_version: SCHEMA_VERSION,
                session_id: "X".into(),
                created: std::time::SystemTime::now(),
                root: PathBuf::from("/"),
                parent: None,
            },
            path: PathBuf::from("/nonexistent"),
            next_offset: 0,
            blobs: BlobStore::new(blob_dir),
            poisoned: AtomicBool::new(false),
            _record: PhantomData,
        }
    }

    #[test]
    fn failed_large_append_poisons_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(
            Failing {
                budget: 10_000,
                sync_fails: false,
            },
            dir.path(),
        );
        w.append(&"small".to_string()).unwrap();
        // Larger than the BufWriter capacity: goes straight to the sink, which
        // accepts part of it and then fails.
        let big = "x".repeat(64 * 1024);
        assert!(matches!(w.append(&big), Err(crate::Error::Io(_))));
        assert!(matches!(
            w.append(&"next".to_string()),
            Err(crate::Error::Poisoned)
        ));
        assert!(matches!(w.checkpoint(), Err(crate::Error::Poisoned)));
        assert!(matches!(w.blob(b"x"), Err(crate::Error::Poisoned)));
    }

    #[test]
    fn failed_checkpoint_poisons_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(
            Failing {
                budget: usize::MAX,
                sync_fails: true,
            },
            dir.path(),
        );
        w.append(&"a".to_string()).unwrap();
        assert!(matches!(w.checkpoint(), Err(crate::Error::Io(_))));
        assert!(matches!(
            w.append(&"b".to_string()),
            Err(crate::Error::Poisoned)
        ));
    }

    #[test]
    fn failed_blob_write_poisons_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        // blobs/ cannot be created beneath a regular file.
        let mut w = writer(
            Failing {
                budget: usize::MAX,
                sync_fails: false,
            },
            &file.join("proj"),
        );
        assert!(matches!(w.blob(b"x"), Err(crate::Error::Io(_))));
        assert!(matches!(
            w.append(&"a".to_string()),
            Err(crate::Error::Poisoned)
        ));
    }
}
