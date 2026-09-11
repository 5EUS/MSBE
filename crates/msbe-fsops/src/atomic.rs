//! Crash-safe placement: stage beside the destination, flush, then rename into place.
//!
//! A reader of the destination sees either the old file or the new one, never a partial
//! write.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
};

use crate::{
    error::{IoResultExt, Result},
    sys,
};

/// Atomically replaces `dest` with `staged`. Both must be on the same volume.
///
/// On Windows this fails if `dest` is read-only. The applier and the store, which have
/// already preserved what they replace, use a variant that can displace a read-only file.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if the rename or the directory flush fails.
pub fn rename_replace(staged: &Path, dest: &Path) -> Result<()> {
    platform_rename(staged, dest).at("rename into place", dest)?;
    sync_parent(dest)
}

/// Replaces `dest` with `staged`, even when `dest` is read-only.
///
/// Windows refuses to rename over a read-only file, and clearing the attribute first is not
/// an option: a hardlink shares its attributes with the store blob it links to, so the blob
/// would become writable. Instead, when the rename is denied and `dest` is read-only, `dest`
/// is removed (Windows permits that for read-only files) and the rename is retried.
///
/// Those two steps are not atomic, so only callers that have already preserved `dest` may use
/// this: the applier journals it first, and the store only displaces a blob it is replacing
/// with verified bytes. Wherever the first rename succeeds, which is every Unix system, the
/// replacement stays atomic.
pub(crate) fn rename_displacing(staged: &Path, dest: &Path) -> Result<()> {
    displace_with(staged, dest, platform_rename)?;
    sync_parent(dest)
}

fn displace_with(
    staged: &Path,
    dest: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    match rename(staged, dest) {
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied && sys::is_read_only_file(dest) =>
        {
            sys::remove_file_if_exists(dest)?;
            rename(staged, dest).at("rename into place", dest)
        }
        renamed => renamed.at("rename into place", dest),
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "this is the sanctioned atomic rename that the workspace ban points to"
)]
fn platform_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
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

#[cfg(test)]
mod tests {
    use std::{fs, io, path::Path};

    use super::{displace_with, platform_rename, rename_displacing};
    use crate::sys;

    /// Renames the way Windows does: replacing a read-only file is denied.
    fn windows_rename(from: &Path, to: &Path) -> io::Result<()> {
        if sys::is_read_only_file(to) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        platform_rename(from, to)
    }

    #[test]
    fn a_read_only_hardlink_is_displaced_without_touching_the_blob_it_shares() {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob");
        fs::write(&blob, b"stored").unwrap();
        sys::make_read_only(&blob).unwrap();

        // Windows semantics simulated on every platform, then the real rename, which is what
        // exercises Windows itself in CI.
        for (index, real) in [false, true].into_iter().enumerate() {
            let dest = dir.path().join(format!("deployed-{index}"));
            fs::hard_link(&blob, &dest).unwrap();
            let staged = dir.path().join(format!("staged-{index}"));
            fs::write(&staged, b"replacement").unwrap();

            if real {
                rename_displacing(&staged, &dest).unwrap();
            } else {
                displace_with(&staged, &dest, windows_rename).unwrap();
            }
            assert_eq!(fs::read(&dest).unwrap(), b"replacement");
            assert!(!staged.exists());
            assert_eq!(fs::read(&blob).unwrap(), b"stored");
            assert!(sys::is_read_only_file(&blob), "the blob became writable");
        }
    }

    #[test]
    fn a_denied_rename_over_a_writable_file_is_reported_not_worked_around() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::write(&dest, b"original").unwrap();
        let staged = dir.path().join("staged");
        fs::write(&staged, b"new").unwrap();

        let denied =
            |_: &Path, _: &Path| -> io::Result<()> { Err(io::ErrorKind::PermissionDenied.into()) };
        assert!(displace_with(&staged, &dest, denied).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"original");
        assert!(staged.exists());
    }
}
