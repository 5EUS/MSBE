//! Crash-safe placement: stage beside the destination, flush, then rename into place.
//!
//! A reader of the destination sees either the old file or the new one, never a partial
//! write.

use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

use crate::{
    error::{IoResultExt, Result},
    sys,
};

/// Atomically replaces `dest` with `staged`. Both must be on the same volume.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if the rename or the directory flush fails.
pub fn rename_replace(staged: &Path, dest: &Path) -> Result<()> {
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the sanctioned atomic rename that the workspace ban points to"
    )]
    let renamed = fs::rename(staged, dest);
    renamed.at("rename into place", dest)?;
    sync_parent(dest)
}

/// Writes `bytes` to `dest` through a staged sibling, an fsync and an atomic rename.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if any step fails. `dest` is untouched unless the final
/// rename succeeds.
pub fn write_file(dest: &Path, bytes: &[u8]) -> Result<()> {
    let staged = sys::sibling(dest, "write");
    sys::remove_file_if_exists(&staged)?;
    let mut file = File::create_new(&staged).at("create", &staged)?;
    file.write_all(bytes).at("write", &staged)?;
    file.sync_all().at("fsync", &staged)?;
    drop(file);
    rename_replace(&staged, dest)
}

/// Flushes the directory containing `path`, so a completed rename survives power loss.
///
/// Directories cannot be flushed this way on Windows, where this does nothing.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if the directory cannot be opened or flushed.
pub fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let dir = File::open(parent).at("open directory", parent)?;
        dir.sync_all().at("fsync directory", parent)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
