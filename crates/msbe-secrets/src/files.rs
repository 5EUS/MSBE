//! The owner-only directory credential state lives in.

use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
};

use msbe_core::config::Home;

use crate::StoreError;

/// The most bytes a credential state file may be.
const FILE_LIMIT: u64 = 1 << 20;

/// The directory in `home` that credential state lives in.
pub fn auth_directory(home: &Home) -> PathBuf {
    home.root().join("auth")
}

/// Reads the `kind` file at `path`, or `None` when it does not exist.
pub(crate) fn read_text(path: &Path, kind: &'static str) -> Result<Option<String>, StoreError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("read", path, source)),
    };
    let mut text = String::new();
    file.take(FILE_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|source| io_error("read", path, source))?;
    if text.len() as u64 > FILE_LIMIT {
        return Err(malformed(path, kind, "it is larger than 1 MiB"));
    }
    Ok(Some(text))
}

/// Replaces `path` with `bytes` atomically, inside a directory only its owner can read.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        private_directory(parent)?;
    }
    msbe_fsops::atomic::write_file(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|source| io_error("secure", path, source))?;
    }
    Ok(())
}

/// Creates `directory` if needed, and makes it readable only by its owner on Unix. The staged
/// file an atomic write creates inside it is therefore never readable by anyone else either.
fn private_directory(directory: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(directory).map_err(|source| io_error("create", directory, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error("secure", directory, source))?;
    }
    Ok(())
}

pub(crate) fn io_error(action: &'static str, path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn malformed(path: &Path, kind: &'static str, reason: impl Into<String>) -> StoreError {
    StoreError::Malformed {
        path: path.to_path_buf(),
        kind,
        reason: reason.into(),
    }
}
