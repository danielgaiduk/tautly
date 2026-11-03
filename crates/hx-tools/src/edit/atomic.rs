//! Encoding-preserving atomic write (spec 5.4 step 3).
//!
//! Matching runs on BOM-less, LF-only text; [`restore`] puts the file's own BOM, line
//! endings and trailing-newline state back before the bytes reach the sandbox, whose
//! `write_atomic` (temp file, fsync, rename) keeps the existing file mode.

use std::path::Path;

use hx_sandbox::Sandbox;

use super::EditError;
use crate::spec::decode_text;

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Decode file bytes into BOM-less, LF-only text for matching. Binary or non-UTF-8
/// content is rejected.
pub fn to_lf_text(original: &[u8]) -> Result<String, EditError> {
    let text = decode_text(original).map_err(EditError::Undecodable)?;
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    Ok(text.replace("\r\n", "\n"))
}

/// The bytes to write for `new_text` (LF-only, BOM-less): `original`'s BOM, line endings
/// and trailing-newline state restored. Files with mixed endings take the majority style.
pub fn restore(original: &[u8], new_text: &str) -> Vec<u8> {
    let has_bom = original.starts_with(BOM);
    let body = if has_bom {
        &original[BOM.len()..]
    } else {
        original
    };
    let crlf = body.windows(2).filter(|w| w == b"\r\n").count();
    let lf = body.iter().filter(|&&b| b == b'\n').count() - crlf;
    let use_crlf = crlf > 0 && crlf >= lf;

    let mut text = new_text.to_string();
    // An empty original has no state to keep; neither does an edit that empties the file.
    if !body.is_empty() && !text.is_empty() {
        match (body.ends_with(b"\n"), text.ends_with('\n')) {
            (true, false) => text.push('\n'),
            (false, true) => {
                text.pop();
            }
            _ => {}
        }
    }
    if use_crlf {
        text = text.replace('\n', "\r\n");
    }
    let mut out = Vec::with_capacity(text.len() + BOM.len());
    if has_bom {
        out.extend_from_slice(BOM);
    }
    out.extend_from_slice(text.as_bytes());
    out
}

/// Write `new_text` over `path`, restoring `original`'s CRLF, BOM, trailing-newline state
/// and file mode.
pub async fn preserve_and_write(
    sb: &dyn Sandbox,
    path: &Path,
    original: &[u8],
    new_text: &str,
) -> Result<(), EditError> {
    sb.write_atomic(path, &restore(original, new_text), None)
        .await?;
    Ok(())
}
