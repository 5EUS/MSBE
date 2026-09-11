//! Probing what a volume actually supports, by attempting it.
//!
//! Filesystem type is not a reliable proxy: in M0 an NTFS volume reported itself as
//! `fuse` (see `docs/15-m0-findings.md`). The only trustworthy answer is to try.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{atomic, error::Result, materialize::Backend, store::Store, sys};

/// What materialization from a store shard into a target directory can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// A copy-on-write clone succeeded (`FICLONE`, `clonefile`, or `ReFS` block cloning).
    pub reflink: bool,
    /// A hardlink succeeded. Hardlinks fail across volumes.
    pub hardlink: bool,
}

impl Capabilities {
    /// Creates a file in the shard and attempts a clone and a hardlink into `target_dir`.
    ///
    /// A failed clone or link is a finding, not an error. Every probe file is removed.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Io`] if a probe file cannot be created or removed.
    pub fn probe(store: &Store, target_dir: &Path) -> Result<Self> {
        let source = store.tmp_dir().join(sys::unique_name("probe"));
        atomic::write_file(&source, b"msbe capability probe")?;
        let clone = target_dir.join(format!(".{}", sys::unique_name("msbe-probe-clone")));
        let link = target_dir.join(format!(".{}", sys::unique_name("msbe-probe-link")));
        let reflink = reflink_copy::reflink(&source, &clone).is_ok();
        let hardlink = fs::hard_link(&source, &link).is_ok();
        sys::remove_file_if_exists(&clone)?;
        sys::remove_file_if_exists(&link)?;
        sys::remove_file_if_exists(&source)?;
        Ok(Self { reflink, hardlink })
    }

    /// Whether `backend` is available. Copy always is.
    pub const fn supports(self, backend: Backend) -> bool {
        match backend {
            Backend::Reflink => self.reflink,
            Backend::Hardlink => self.hardlink,
            Backend::Copy => true,
        }
    }

    /// The first backend in `preference` that this volume supports, or copy.
    pub fn choose(self, preference: &[Backend]) -> Backend {
        preference
            .iter()
            .copied()
            .find(|backend| self.supports(*backend))
            .unwrap_or(Backend::Copy)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Capabilities;
    use crate::{Backend, Store, sys};

    #[test]
    fn probe_agrees_with_a_manual_hardlink_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();

        let capabilities = Capabilities::probe(&store, &target).unwrap();

        let src = dir.path().join("src");
        fs::write(&src, b"x").unwrap();
        let manual = target.join("manual");
        assert_eq!(capabilities.hardlink, fs::hard_link(&src, &manual).is_ok());
        sys::remove_file_if_exists(&manual).unwrap();
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "probe files must be removed"
        );
    }

    #[test]
    fn choose_skips_unsupported_backends_and_falls_back_to_copy() {
        let nothing = Capabilities {
            reflink: false,
            hardlink: false,
        };
        assert_eq!(nothing.choose(&Backend::DEFAULT_CHAIN), Backend::Copy);
        let links = Capabilities {
            reflink: false,
            hardlink: true,
        };
        assert_eq!(links.choose(&Backend::DEFAULT_CHAIN), Backend::Hardlink);
        assert_eq!(links.choose(&[]), Backend::Copy);
    }
}
