//! The operations the applier performs, and what each one displaced.

use serde::{Deserialize, Serialize};

use crate::{digest::Digest, relpath::RelPath};

/// A single filesystem mutation the applier can perform and undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Ensures a directory exists. The applier inserts these for missing parents.
    CreateDir {
        /// The directory.
        path: RelPath,
    },
    /// Places a blob from the store at a path, replacing any file already there.
    Materialize {
        /// The destination.
        path: RelPath,
        /// The blob to place.
        blob: Digest,
        /// Whether the game or a mod rewrites this file at runtime. Mutable files are
        /// always copied, never linked, so a write cannot reach the shared store blob.
        mutable: bool,
    },
    /// Removes a file.
    Remove {
        /// The file.
        path: RelPath,
    },
    /// Removes a directory if it is empty. One that still holds anything is left in place, so
    /// nothing unmanaged inside it is ever deleted.
    RemoveDir {
        /// The directory.
        path: RelPath,
    },
}

impl Operation {
    /// The path the operation acts on.
    pub fn path(&self) -> &RelPath {
        match self {
            Self::CreateDir { path }
            | Self::Materialize { path, .. }
            | Self::Remove { path }
            | Self::RemoveDir { path } => path,
        }
    }
}

/// What occupied a path before an operation touched it. Rollback restores exactly this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prior {
    /// Nothing was there.
    Absent,
    /// A regular file, preserved in the store before the operation ran.
    File {
        /// The preserved contents.
        digest: Digest,
        /// Whether an execute bit was set, so a restored native executable still runs.
        executable: bool,
        /// Whether the file was read-only, so restoring it restores that too. Absent from
        /// journals written before this was recorded.
        #[serde(default)]
        read_only: bool,
    },
    /// A directory was already there.
    Dir,
}
