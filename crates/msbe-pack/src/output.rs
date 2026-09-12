//! Crash-safe container output shared by exports and snapshots.

use std::{fs, path::Path};

use msbe_archive::Limits;
use msbe_fsops::{Digest, RelPath, Store};
use msbe_provider_api::{PackInput, PackLayout};

use crate::{
    IssueCode, PackError, Progress,
    error::io_error,
    host::{self, Compression, Written, ZipPackInput},
    progress::checkpoint,
};

/// Writes `layout` to `output` and returns the digest of the written file.
///
/// The container is written beside its destination under a temporary name while every blob is
/// hashed, reopened and checked to hold exactly the layout's entries, and only then renamed into
/// place. A failure or cancellation leaves `output` untouched.
pub(crate) fn write(
    layout: &PackLayout,
    store: &Store,
    compression: Compression,
    output: &Path,
    progress: &dyn Progress,
) -> Result<Digest, PackError> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(".msbe-output-")
        .tempfile_in(parent)
        .map_err(io_error("create a temporary file in", parent))?;
    let written = host::write_zip_layout(
        layout,
        store,
        compression,
        staged.as_file_mut(),
        &mut |done, total| {
            progress.report(count(done), count(total), "writing");
            !progress.cancelled()
        },
    )?;
    if written == Written::Cancelled {
        return Err(PackError::Cancelled);
    }
    staged
        .as_file()
        .sync_all()
        .map_err(io_error("flush", staged.path()))?;
    let reopened = ZipPackInput::open(staged.path(), &Limits::default())?;
    let mut expected: Vec<&RelPath> = layout.entries.iter().map(|entry| &entry.path).collect();
    expected.sort();
    if reopened
        .entries()
        .iter()
        .map(|entry| &entry.path)
        .ne(expected.iter().copied())
    {
        return Err(PackError::issue(
            IssueCode::IntegrityMismatch,
            "the written container does not hold exactly the planned entries",
        ));
    }
    checkpoint(progress)?;
    staged
        .persist(output)
        .map_err(|error| io_error("replace", output)(error.error))?;
    fs::File::open(output)
        .and_then(|file| file.sync_all())
        .map_err(io_error("flush", output))?;
    Ok(reopened.digest())
}

fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
