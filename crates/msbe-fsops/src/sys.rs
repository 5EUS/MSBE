//! The only place this crate removes files, plus small shared helpers.
//!
//! Destructive calls are banned workspace-wide in `clippy.toml`. They are permitted here,
//! once each, because every caller journals the prior state before removing anything.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::error::{Error, IoResultExt, Result};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name unique within this process, and distinct from other processes' names.
pub(crate) fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A hidden sibling of `target` tagged with `tag`, used for staging and restores.
pub(crate) fn sibling(target: &Path, tag: &str) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(target.file_name().unwrap_or_default());
    name.push(".msbe-");
    name.push(tag);
    target.with_file_name(name)
}

/// Removes a file, treating "already gone" as success. Returns whether it existed.
pub(crate) fn remove_file_if_exists(path: &Path) -> Result<bool> {
    #[expect(
        clippy::disallowed_methods,
        reason = "callers journal the prior state before removing anything"
    )]
    let removed = fs::remove_file(path);
    match removed {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(Error::Io {
            op: "remove",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Marks a file read-only: every write bit cleared on Unix, the read-only attribute on Windows.
pub(crate) fn make_read_only(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path).at("stat", path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).at("set permissions", path)
}

/// Whether `path` is a regular file marked read-only. Anything unreadable counts as not.
pub(crate) fn is_read_only_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().readonly())
}

/// Removes a directory if it is empty. A missing or non-empty directory is left alone:
/// something unmanaged inside it is never deleted.
pub(crate) fn remove_dir_if_empty(path: &Path) -> Result<bool> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(false)
        }
        Err(source) => Err(Error::Io {
            op: "remove directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}
