//! How a blob's bytes reach a destination path.

use std::{
    fs::{self, File},
    io,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{IoResultExt, Result},
    sys,
};

/// How a blob is materialized at its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// A copy-on-write clone: cheap, and isolated from the store.
    Reflink,
    /// A hardlink to the read-only store blob: cheap, same volume only, shared inode.
    Hardlink,
    /// A full copy: always works, costs disk, and is writable.
    Copy,
}

impl Backend {
    /// The default preference chain from `docs/04-deployment-engine.md` §4.2.
    pub const DEFAULT_CHAIN: [Self; 3] = [Self::Reflink, Self::Hardlink, Self::Copy];
}

/// Creates `staged` with the blob's contents using `wanted`, falling down the chain
/// (reflink, hardlink, copy) if a backend fails at runtime, and returns the backend actually
/// used. A leftover file at `staged` from an earlier crash is replaced.
pub(crate) fn stage(blob: &Path, staged: &Path, wanted: Backend) -> Result<Backend> {
    sys::remove_file_if_exists(staged)?;
    if wanted == Backend::Reflink {
        if reflink_copy::reflink(blob, staged).is_ok() {
            return Ok(Backend::Reflink);
        }
        sys::remove_file_if_exists(staged)?;
    }
    if wanted != Backend::Copy {
        if fs::hard_link(blob, staged).is_ok() {
            return Ok(Backend::Hardlink);
        }
        sys::remove_file_if_exists(staged)?;
    }
    copy_synced(blob, staged)?;
    Ok(Backend::Copy)
}

/// Copies into a newly created file, so the copy gets default (writable) permissions rather
/// than inheriting the store blob's read-only mode.
fn copy_synced(from: &Path, to: &Path) -> Result<()> {
    let mut input = File::open(from).at("open", from)?;
    let mut output = File::create_new(to).at("create", to)?;
    io::copy(&mut input, &mut output).at("copy", to)?;
    output.sync_all().at("fsync", to)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Backend, stage};

    #[test]
    fn a_copy_is_independent_of_its_blob() {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob");
        fs::write(&blob, b"shared").unwrap();
        let staged = dir.path().join("staged");
        assert_eq!(stage(&blob, &staged, Backend::Copy).unwrap(), Backend::Copy);
        fs::write(&staged, b"changed").unwrap();
        assert_eq!(fs::read(&blob).unwrap(), b"shared");
    }

    #[test]
    fn replaces_a_stale_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob");
        fs::write(&blob, b"fresh").unwrap();
        let staged = dir.path().join("staged");
        fs::write(&staged, b"left over from a crash").unwrap();
        stage(&blob, &staged, Backend::Hardlink).unwrap();
        assert_eq!(fs::read(&staged).unwrap(), b"fresh");
    }
}
