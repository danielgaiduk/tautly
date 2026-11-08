use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::marker::PhantomData;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use ulid::Ulid;

use crate::blobs::{BlobRef, BlobStore};
use crate::git;
use crate::paths;
use crate::reader::SessionReader;
use crate::record::{GitState, Header, HeaderInit, LineRef, SCHEMA_VERSION};
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

/// What a closed writer writes to: nothing.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the session log is closed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Sink for Closed {
    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Take an exclusive, non-blocking advisory lock held until `file` is dropped. A lock
/// that is busy is tried again a few times over ~50 ms first: a reader that only looks
/// (the session tree's liveness probe holds a shared lock for an instant) must not make an
/// open fail.
fn lock_exclusive(file: &File, path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    for attempt in 0..5 {
        // SAFETY: plain syscall on a file descriptor owned by `file`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err.into());
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Err(Error::AlreadyOpen(path.to_path_buf()))
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
    /// The `t` of the last record written: `t` never goes backwards within a log.
    last_t: u64,
    blobs: BlobStore,
    poisoned: AtomicBool,
    _record: PhantomData<fn(&R)>,
}

impl<R: Serialize> SessionWriter<R> {
    /// Create a new session for the project at `root` under [`paths::data_home`], with
    /// the header fields `init` decides (a fork's parent, label, origin, model, ...).
    pub fn create(root: &Path, init: HeaderInit) -> Result<Self> {
        Self::create_in(&paths::data_home()?, root, init)
    }

    /// Like [`create`](Self::create) with an explicit data home. The log goes in the
    /// directory of the root's [`project_id`](paths::project_id), shared by every
    /// worktree of a repository. The git state is read from files (no git is run).
    pub fn create_in(data_home: &Path, root: &Path, init: HeaderInit) -> Result<Self> {
        let header = new_header(Ulid::new(), root, init);
        let dir = paths::sessions_dir(data_home, &header.project);
        Self::create_at(dir, header)
    }

    /// A new session for the work tree at `root` whose log goes next to the log at
    /// `beside` (a fork's parent), so that the two share the blob store. `root` must
    /// belong to the same project ([`project_id`](paths::project_id); another worktree of
    /// the repository, say), else [`Error::OtherProject`]. `id` is the session id to use
    /// (a ULID, [`new_session_id`]; none: a new one), so a caller can name things after
    /// the session before it exists.
    pub fn create_beside(
        beside: &Path,
        root: &Path,
        id: Option<&str>,
        init: HeaderInit,
    ) -> Result<Self> {
        let ulid = match id {
            Some(id) => Ulid::from_string(id).map_err(|_| Error::InvalidSessionId(id.into()))?,
            None => Ulid::new(),
        };
        let header = new_header(ulid, root, init);
        let dir = beside.parent().unwrap_or(Path::new("."));
        if dir.file_name().and_then(|n| n.to_str()) != Some(header.project.as_str()) {
            return Err(Error::OtherProject {
                root: header.root,
                log: beside.to_path_buf(),
            });
        }
        Self::create_at(dir.to_path_buf(), header)
    }

    /// Create the log of `header` in the project directory `dir`.
    fn create_at(dir: PathBuf, header: Header) -> Result<Self> {
        paths::create_private_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", header.session_id));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        lock_exclusive(&file, &path)?;
        let mut w = Self::from_file(file, header, path, 0, 0, BlobStore::new(&dir));
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
        last_t: u64,
        blobs: BlobStore,
    ) -> Self {
        Self {
            out: BufWriter::new(Box::new(file)),
            header,
            path,
            next_offset,
            last_t,
            blobs,
            poisoned: AtomicBool::new(false),
            _record: PhantomData,
        }
    }

    /// The `t` to write for a record made at `t_ms`: never before the previous record's,
    /// so `t` is non-decreasing within a log even when the clock steps back.
    fn stamp(&mut self, t_ms: u64) -> u64 {
        self.last_t = self.last_t.max(t_ms);
        self.last_t
    }

    /// Copy the first `count` records of the log at `src` (a fork's parent) to the end of
    /// this one, verbatim: each record's JSON is copied byte for byte (fields a newer
    /// version added survive) and keeps its original `t`; offsets continue this log's.
    /// Returns how many were copied (fewer when `src` has fewer). Buffered like
    /// [`append`](Self::append).
    pub fn copy_from(&mut self, src: &Path, count: u64) -> Result<u64> {
        self.check_usable()?;
        let reader = SessionReader::<Box<RawValue>>::open(src)?;
        let mut copied = 0;
        for entry in reader
            .entries()
            .take(usize::try_from(count).unwrap_or(usize::MAX))
        {
            let entry = entry?;
            let line = encode(&LineRef::Record {
                offset: self.next_offset,
                t: self.stamp(entry.t_ms),
                data: &entry.rec,
            })?;
            self.write_line(&line)?;
            self.next_offset += 1;
            copied += 1;
        }
        Ok(copied)
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

    /// Buffer one record, stamped with the current time (clamped so `t` never goes back);
    /// returns its 0-based offset.
    pub fn append(&mut self, rec: &R) -> Result<u64> {
        let t = now_ms();
        self.append_stamped(rec, t)
    }

    /// [`append`](Self::append), stamped with the previous record's time instead of now:
    /// for a record written on behalf of the past (a synthetic turn end).
    pub fn append_at_last(&mut self, rec: &R) -> Result<u64> {
        self.append_stamped(rec, 0)
    }

    fn append_stamped(&mut self, rec: &R, t_ms: u64) -> Result<u64> {
        self.check_usable()?;
        let offset = self.next_offset;
        let t = self.stamp(t_ms);
        let line = encode(&LineRef::Record {
            offset,
            t,
            data: rec,
        })?;
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

    /// Flush and fsync the log, then close its file, which releases the writer lock at
    /// once (not whenever the last handle to the writer goes): a client that resumes the
    /// session right after it ended finds it free. Every later call fails as on a
    /// poisoned writer.
    pub fn close(&mut self) -> Result<()> {
        let synced = self.checkpoint();
        let old = std::mem::replace(&mut self.out, BufWriter::new(Box::new(Closed)));
        // Already flushed; the file (and its lock) goes here.
        drop(old.into_parts());
        self.poisoned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        synced
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
        let (mut count, mut last_t) = (0, 0);
        for entry in reader.entries() {
            last_t = last_t.max(entry?.t_ms);
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
            last_t,
            BlobStore::new(dir),
        ))
    }
}

/// The header of a new session `ulid` at `root`. The git directories are resolved once,
/// for both the project id and the head.
fn new_header(ulid: Ulid, root: &Path, init: HeaderInit) -> Header {
    let dirs = git::dirs(root);
    let git = dirs.as_ref().map(|d| GitState {
        head: git::head(d),
        dirty: None,
    });
    Header {
        schema_version: SCHEMA_VERSION,
        session_id: ulid.to_string(),
        created: ulid.datetime(),
        root: paths::canonical_root(root),
        parent: init.parent,
        project: paths::project_id_with(root, dirs.as_ref()),
        label: init.label,
        origin: init.origin,
        git,
        tautly_version: env!("CARGO_PKG_VERSION").to_string(),
        model: init.model,
        config_hash: init.config_hash,
        tags: init.tags,
        parent_step: init.parent_step,
    }
}

/// A new session id (a ULID), for [`SessionWriter::create_beside`].
pub fn new_session_id() -> String {
    Ulid::new().to_string()
}

/// Unix milliseconds now (0 before 1970, which only a broken clock gives).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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
            header: new_header(Ulid::new(), Path::new("/"), HeaderInit::default()),
            path: PathBuf::from("/nonexistent"),
            next_offset: 0,
            last_t: 0,
            blobs: BlobStore::new(blob_dir),
            poisoned: AtomicBool::new(false),
            _record: PhantomData,
        }
    }

    #[test]
    fn reopen_waits_out_a_brief_shared_lock() {
        use std::os::fd::AsRawFd;
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let w = SessionWriter::<String>::create_in(home.path(), root.path(), HeaderInit::default())
            .unwrap();
        let path = w.path().to_path_buf();
        drop(w);
        // A liveness probe: a shared lock held for an instant.
        let probe = File::open(&path).unwrap();
        // SAFETY: flock on a file descriptor we own.
        assert_eq!(unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH) }, 0);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(15));
            drop(probe);
        });
        assert!(SessionWriter::<String>::reopen(&path).is_ok());
        release.join().unwrap();
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
