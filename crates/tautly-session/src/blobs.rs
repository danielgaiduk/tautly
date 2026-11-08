use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Content address of a blob: the blake3 hex digest (64 chars) of its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobRef(String);

impl BlobRef {
    /// The hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap a digest string, rejecting anything that is not 64 lowercase hex
    /// digits (so a reference read from a log can never escape `blobs/`).
    pub fn parse(s: &str) -> Result<Self> {
        let ok = s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(Self(s.to_string()))
        } else {
            Err(Error::InvalidBlobRef(s.to_string()))
        }
    }
}

/// The `blobs/` directory next to a project's session logs.
pub(crate) struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub(crate) fn new(project_dir: &Path) -> Self {
        Self {
            dir: project_dir.join("blobs"),
        }
    }

    fn path_of(&self, r: &BlobRef) -> Result<PathBuf> {
        // Re-validate: a deserialized BlobRef bypasses `parse`.
        let r = BlobRef::parse(r.as_str())?;
        let (head, rest) = r.as_str().split_at(2);
        Ok(self.dir.join(head).join(rest))
    }

    /// Store `bytes`; written atomically (temp file + rename) and only if
    /// absent.
    pub(crate) fn put(&self, bytes: &[u8]) -> Result<BlobRef> {
        let r = BlobRef(blake3::hash(bytes).to_hex().to_string());
        let path = self.path_of(&r)?;
        if path.exists() {
            return Ok(r);
        }
        // Records reference this blob; it must be durable first.
        crate::files::write_private_atomic(&path, bytes, true)?;
        Ok(r)
    }

    pub(crate) fn get(&self, r: &BlobRef) -> Result<Vec<u8>> {
        match fs::read(self.path_of(r)?) {
            Ok(b) if blake3::hash(&b).to_hex().as_str() == r.as_str() => Ok(b),
            Ok(_) => Err(Error::BlobCorrupt(r.as_str().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::BlobNotFound(r.as_str().to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }
}
