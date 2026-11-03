use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;
use ulid::Ulid;

use crate::Result;
use crate::blobs::{BlobRef, BlobStore};
use crate::paths;
use crate::record::{Header, LineRef, SCHEMA_VERSION};

/// Appends records to one session log.
///
/// `append` only buffers; call [`checkpoint`](Self::checkpoint) at turn and
/// tool boundaries to flush and fsync. Dropping the writer flushes the buffer
/// (without fsync).
pub struct SessionWriter<R: Serialize> {
    out: BufWriter<File>,
    header: Header,
    path: PathBuf,
    next_offset: u64,
    blobs: BlobStore,
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
        fs::create_dir_all(&dir)?;
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
            .open(&path)?;
        let mut w = Self {
            out: BufWriter::new(file),
            header,
            path,
            next_offset: 0,
            blobs: BlobStore::new(&dir),
            _record: PhantomData,
        };
        let line = encode(&LineRef::<R>::Header(&w.header))?;
        w.out.write_all(&line)?;
        w.checkpoint()?;
        Ok(w)
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

    /// Buffer one record; returns its 0-based offset.
    pub fn append(&mut self, rec: &R) -> Result<u64> {
        let offset = self.next_offset;
        let line = encode(&LineRef::Record { offset, data: rec })?;
        self.out.write_all(&line)?;
        self.next_offset += 1;
        Ok(offset)
    }

    /// Flush buffered lines and fsync the log.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_data()?;
        Ok(())
    }

    /// Store `bytes` content-addressed; identical bytes are stored once.
    pub fn blob(&self, bytes: &[u8]) -> Result<BlobRef> {
        self.blobs.put(bytes)
    }
}

fn encode<R: Serialize>(line: &LineRef<'_, R>) -> Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(line)?;
    buf.push(b'\n');
    Ok(buf)
}
