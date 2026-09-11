//! The crate's error type.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::{applier::Checkpoint, digest::Digest, journal::TxnId};

/// Shorthand for results carrying this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong in the store, journal or applier.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A filesystem call failed.
    #[error("{op} {}: {source}", .path.display())]
    Io {
        /// What was being attempted, such as `"open"` or `"fsync"`.
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// A relative path failed validation.
    #[error("invalid relative path {path:?}: {reason}")]
    InvalidPath {
        /// The rejected input.
        path: String,
        /// The rule it broke.
        reason: &'static str,
    },

    /// A path resolves outside the instance root, or is a symlink or special file.
    #[error("{} escapes the instance root or is not a regular file or directory", .path.display())]
    EscapesRoot {
        /// The offending path.
        path: PathBuf,
    },

    /// An operation would replace or remove a directory.
    #[error("{} is a directory; operations never replace or remove directories", .path.display())]
    IsDirectory {
        /// The directory.
        path: PathBuf,
    },

    /// A path needed as a directory is a file.
    #[error("{} is a file where a directory is needed", .path.display())]
    NotADirectory {
        /// The file.
        path: PathBuf,
    },

    /// A transaction removes a directory while also creating or placing something inside it.
    #[error("{} is inside a directory the same transaction removes", .path.display())]
    DirectoryInUse {
        /// The path inside the removed directory.
        path: PathBuf,
    },

    /// A blob's contents no longer match its digest.
    #[error("blob {expected} is corrupt: its contents hash to {actual}")]
    Corrupt {
        /// The digest the blob is stored under.
        expected: Digest,
        /// What its contents hash to now.
        actual: Digest,
    },

    /// A blob is referenced but not present in the store.
    #[error("blob {0} is not in the store")]
    MissingBlob(Digest),

    /// A digest string was malformed.
    #[error("invalid digest {0:?}")]
    InvalidDigest(String),

    /// The journal has an unreadable record before its final line.
    #[error("journal {} is corrupt at line {line}: {reason}", .path.display())]
    JournalCorrupt {
        /// The journal file.
        path: PathBuf,
        /// The 1-based line number.
        line: usize,
        /// The decoder's message.
        reason: String,
    },

    /// A journal record could not be encoded.
    #[error("cannot encode journal record: {0}")]
    Encode(#[from] serde_json::Error),

    /// A transaction is still open; [`crate::Applier::recover`] must run first.
    #[error("an interrupted transaction must be recovered before anything else is applied")]
    RecoveryRequired,

    /// Only the most recent live transaction can be rolled back.
    #[error("transaction {txn} is not the most recent live transaction")]
    NotLatest {
        /// The transaction that was asked for.
        txn: TxnId,
    },

    /// The transaction never committed, or was already rolled back.
    #[error("transaction {txn} is not a committed, live transaction")]
    NotCommitted {
        /// The transaction that was asked for.
        txn: TxnId,
    },

    /// An observer stopped the transaction at a checkpoint.
    #[error("apply aborted at {0:?}")]
    Aborted(Checkpoint),
}

/// Attaches an operation name and path to I/O errors.
pub(crate) trait IoResultExt<T> {
    /// Converts an [`io::Error`] into [`Error::Io`].
    fn at(self, op: &'static str, path: &Path) -> Result<T>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn at(self, op: &'static str, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            op,
            path: path.to_path_buf(),
            source,
        })
    }
}
