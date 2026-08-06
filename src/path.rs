//! Canonicalization that produces paths external tools can actually use.

use std::path::{Path, PathBuf};

/// Canonicalize a path, keeping the result in a form other programs accept.
///
/// On Windows, [`std::fs::canonicalize`] returns verbatim extended-length
/// paths such as `\\?\C:\work`. Git cannot open those — it rewrites them to
/// `//?/C:/work` and fails — so a plain drive-letter path is restored. UNC
/// shares and device namespaces keep their verbatim prefix, which is required
/// for them to resolve at all.
///
/// Every canonicalization in this crate goes through here, so stored paths and
/// the identity comparisons made against them always share one representation.
pub(crate) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(simplify)
}

#[cfg(windows)]
fn simplify(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return path;
    };
    let bytes = rest.as_bytes();
    let drive_rooted = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'\\');
    if drive_rooted {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn simplify(path: PathBuf) -> PathBuf {
    path
}
