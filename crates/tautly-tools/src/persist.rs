//! Serialized forms of what tools report: lowercase hex for hashes, interned error kinds,
//! and [`PersistedEffect`], the session-log form of an [`AppliedEffect`] that keeps file
//! contents out of the log.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tautly_session::{BlobRef, ConfineError, RelPath};

use crate::edit::tolerant::RepairStage;
use crate::{AppliedEffect, LineRange, Saved, Shown, TestCounts, ToolMeta};

/// `#[serde(with = "...")]` for a `blake3::Hash`: a 64-digit lowercase hex string.
pub(crate) mod hash_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(hash: &blake3::Hash, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(hash.to_hex().as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<blake3::Hash, D::Error> {
        let text = String::deserialize(d)?;
        // `Hash::from_hex` also takes upper case; the stored form is lower case only.
        if text.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(serde::de::Error::custom("hash must be lowercase hex"));
        }
        blake3::Hash::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// `#[serde(with = "...")]` for file contents as base64 (the encoding tautly-provider uses for
/// image data).
pub(crate) mod b64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// Like [`b64`], for an optional value (`null` = none).
pub(crate) mod b64_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match data {
            Some(d) => super::b64::serialize(d, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        #[derive(Deserialize)]
        struct Wrapped(#[serde(with = "super::b64")] Vec<u8>);
        Ok(Option::<Wrapped>::deserialize(d)?.map(|w| w.0))
    }
}

/// Every [`ToolMeta::error_kind`] value a built-in tool sets. `ToolMeta` holds a
/// `&'static str`, which a deserializer cannot produce from text it does not own, so a kind
/// read back from a log is looked up here.
pub(crate) const ERROR_KINDS: &[&str] = &[
    "bad_args",
    "io",
    "undecodable",
    "not_read",
    "stale",
    "not_found",
    "ambiguous",
    "mixed_artifacts",
    "syntax",
    "stale_anchor",
    "invalid_ops",
    "invalid_patch",
    "invalid_memory",
    "changed_after_approval",
];

/// What a kind this version does not know is read back as.
const UNKNOWN_KIND: &str = "unknown";

/// What [`ToolMeta`] is read from. (Deriving `Deserialize` on `ToolMeta` itself would demand
/// `'de: 'static` because of the `&'static str` field.)
#[derive(Deserialize)]
struct MetaRepr {
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    counts: Option<TestCounts>,
    #[serde(default)]
    saved: Option<Saved>,
    #[serde(default)]
    repair: Option<String>,
}

impl<'de> Deserialize<'de> for ToolMeta {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r = MetaRepr::deserialize(d)?;
        Ok(ToolMeta {
            exit_code: r.exit_code,
            error_kind: r.error_kind.map(|k| {
                ERROR_KINDS
                    .iter()
                    .copied()
                    .find(|known| *known == k)
                    .unwrap_or(UNKNOWN_KIND)
            }),
            counts: r.counts,
            saved: r.saved,
            repair: r
                .repair
                .and_then(|n| RepairStage::ALL.into_iter().find(|s| s.name() == n)),
        })
    }
}

/// An [`AppliedEffect`] as the session log stores it: paths are relative to the session root
/// (spec 6.5 H2), and file contents live in the session's blob store, the record holding only
/// their hash (`pre_image_blob`, `post_image_blob`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersistedEffect {
    Read {
        path: RelPath,
        range: LineRange,
        #[serde(with = "hash_hex")]
        file_hash: blake3::Hash,
        shown: Shown,
    },
    Edited {
        path: RelPath,
        pre_image_blob: Option<String>,
        pre_mode: Option<u32>,
        #[serde(with = "hash_hex")]
        post_hash: blake3::Hash,
        /// The content written (hash `post_hash`), so an undone or rewound state can be
        /// restored again (H7). None when it could not be captured. Set by the engine,
        /// which reads it back after the edit; [`from_applied`](Self::from_applied) leaves
        /// it empty.
        #[serde(default)]
        post_image_blob: Option<BlobRef>,
    },
    Deleted {
        path: RelPath,
        pre_image_blob: String,
        pre_mode: u32,
    },
    Exec {
        may_write: bool,
    },
    /// An effect type this version does not know (read back from a newer log; never
    /// written). [`to_applied`](Self::to_applied) treats it as a command that may have
    /// written, which forgets what was known about the files.
    #[serde(other)]
    Unknown,
}

impl PersistedEffect {
    /// Store the effect's file contents with `put` (bytes in, blob reference out) and keep the
    /// references, with its path made relative to `root`. `Ok(None)` when the path cannot be
    /// stored (not below `root`, or not valid UTF-8); nothing is put then. An error from `put`
    /// aborts the conversion.
    pub fn from_applied<E>(
        effect: &AppliedEffect,
        root: &Path,
        put: &mut dyn FnMut(&[u8]) -> Result<String, E>,
    ) -> Result<Option<Self>, E> {
        let rel = |p: &Path| RelPath::from_abs(root, p);
        Ok(Some(match effect {
            AppliedEffect::Read {
                path,
                range,
                file_hash,
                shown,
            } => {
                let Some(path) = rel(path) else {
                    return Ok(None);
                };
                Self::Read {
                    path,
                    range: *range,
                    file_hash: *file_hash,
                    shown: *shown,
                }
            }
            AppliedEffect::Edited {
                path,
                pre_image,
                pre_mode,
                post_hash,
            } => {
                let Some(path) = rel(path) else {
                    return Ok(None);
                };
                Self::Edited {
                    path,
                    pre_image_blob: pre_image.as_deref().map(&mut *put).transpose()?,
                    pre_mode: *pre_mode,
                    post_hash: *post_hash,
                    post_image_blob: None,
                }
            }
            AppliedEffect::Deleted {
                path,
                pre_image,
                pre_mode,
            } => {
                let Some(path) = rel(path) else {
                    return Ok(None);
                };
                Self::Deleted {
                    path,
                    pre_image_blob: put(pre_image)?,
                    pre_mode: *pre_mode,
                }
            }
            AppliedEffect::Exec { may_write } => Self::Exec {
                may_write: *may_write,
            },
        }))
    }

    /// The root-relative path the effect names (none for `Exec` and `Unknown`).
    pub fn path(&self) -> Option<&RelPath> {
        match self {
            Self::Read { path, .. } | Self::Edited { path, .. } | Self::Deleted { path, .. } => {
                Some(path)
            }
            Self::Exec { .. } | Self::Unknown => None,
        }
    }

    /// Rebuild the effect under `root` (the path re-confined there: a path that would leave
    /// it is an error), reading its file contents back with `get` (blob reference in, bytes
    /// out). An error from `get` aborts the conversion.
    pub fn to_applied<E: From<ConfineError>>(
        &self,
        root: &Path,
        get: &mut dyn FnMut(&str) -> Result<Vec<u8>, E>,
    ) -> Result<AppliedEffect, E> {
        Ok(match self {
            Self::Read {
                path,
                range,
                file_hash,
                shown,
            } => AppliedEffect::Read {
                path: path.resolve(root)?,
                range: *range,
                file_hash: *file_hash,
                shown: *shown,
            },
            Self::Edited {
                path,
                pre_image_blob,
                pre_mode,
                post_hash,
                post_image_blob: _,
            } => AppliedEffect::Edited {
                path: path.resolve(root)?,
                pre_image: pre_image_blob.as_deref().map(&mut *get).transpose()?,
                pre_mode: *pre_mode,
                post_hash: *post_hash,
            },
            Self::Deleted {
                path,
                pre_image_blob,
                pre_mode,
            } => AppliedEffect::Deleted {
                path: path.resolve(root)?,
                pre_image: get(pre_image_blob)?,
                pre_mode: *pre_mode,
            },
            Self::Exec { may_write } => AppliedEffect::Exec {
                may_write: *may_write,
            },
            Self::Unknown => AppliedEffect::Exec { may_write: true },
        })
    }
}
