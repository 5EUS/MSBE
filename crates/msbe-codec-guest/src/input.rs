//! A pack's contents, read only through the host's bounded imports.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeMap;

use serde::Deserialize;

use crate::Error;

/// How the host framed the pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// A ZIP archive.
    Zip,
    /// A directory tree.
    Directory,
    /// One unframed file.
    File,
}

/// One entry the host validated: a safe relative path and its declared size.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The normalized entry path.
    pub path: String,
    /// The declared uncompressed size.
    pub size: u64,
}

/// The pack being probed or imported.
///
/// Inside the sandbox every read crosses to the host, which enforces the manifest ceiling and a
/// per-call read budget no matter what `limit` a codec asks for.
#[derive(Debug)]
pub struct Input {
    #[cfg(not(target_arch = "wasm32"))]
    files: BTreeMap<String, Vec<u8>>,
}

impl Input {
    /// The pack the host is serving this call.
    #[cfg(target_arch = "wasm32")]
    #[doc(hidden)]
    pub const fn host() -> Self {
        Self {}
    }

    /// A ZIP-framed pack held in memory, for testing a codec natively.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn memory(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        Self {
            files: files.into_iter().collect(),
        }
    }

    /// How the host framed the pack.
    pub fn container(&self) -> Container {
        #[cfg(target_arch = "wasm32")]
        {
            crate::abi::container()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Container::Zip
        }
    }

    /// Every entry, in lexical path order.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the host cannot list the pack.
    pub fn entries(&self) -> Result<Vec<Entry>, Error> {
        #[cfg(target_arch = "wasm32")]
        {
            let bytes = crate::abi::entries()?;
            serde_json::from_slice(&bytes).map_err(Error::codec)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Ok(self
                .files
                .iter()
                .map(|(path, bytes)| Entry {
                    path: path.clone(),
                    size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                })
                .collect())
        }
    }

    /// Whether the pack holds an entry at `path`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the host cannot list the pack.
    pub fn contains(&self, path: &str) -> Result<bool, Error> {
        Ok(self.entries()?.iter().any(|entry| entry.path == path))
    }

    /// Reads the entry at `path`, failing rather than returning more than `limit` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::FormatMismatch`] for a missing entry, [`Error::Limit`] for one over the
    /// limit or the host's budget, and [`Error::UnsafePath`] for an unsafe path.
    pub fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, Error> {
        #[cfg(target_arch = "wasm32")]
        {
            crate::abi::read(path, limit)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let bytes = self.files.get(path).ok_or(Error::FormatMismatch)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
                return Err(Error::Limit {
                    message: format!("{path} exceeds the {limit}-byte read limit"),
                });
            }
            Ok(bytes.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, Input};

    #[test]
    fn memory_input_lists_and_bounds_reads() {
        let input = Input::memory([("a.json".to_owned(), b"{}".to_vec())]);
        assert_eq!(input.entries().unwrap().len(), 1);
        assert!(input.contains("a.json").unwrap());
        assert_eq!(input.read("a.json", 2).unwrap(), b"{}");
        assert!(matches!(input.read("a.json", 1), Err(Error::Limit { .. })));
        assert_eq!(input.read("b.json", 10), Err(Error::FormatMismatch));
    }
}
