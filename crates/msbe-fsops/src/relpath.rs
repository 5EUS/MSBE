//! Validated, platform-independent relative paths.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A relative path that is lexically confined to its root.
///
/// Stored with `/` separators and validated by the same rules on every platform, so a
/// lockfile written on Linux is valid on Windows: no absolute paths, no `..`, no empty
/// components, and no backslash, colon or NUL. `.` components are normalized away.
///
/// Lexical confinement is necessary but not sufficient. The applier also refuses to
/// follow a symlink out of the instance root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Validates and normalizes `raw`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPath`] naming the rule `raw` broke.
    pub fn new(raw: &str) -> Result<Self> {
        let reject = |reason: &'static str| Error::InvalidPath {
            path: raw.to_owned(),
            reason,
        };
        if raw.starts_with('/') {
            return Err(reject("absolute path"));
        }
        if raw.contains('\\') {
            return Err(reject("backslash separator"));
        }
        if raw.contains(':') {
            return Err(reject("colon (drive letter or alternate data stream)"));
        }
        if raw.contains('\0') {
            return Err(reject("NUL byte"));
        }
        let mut parts = Vec::new();
        for part in raw.split('/') {
            match part {
                "" => return Err(reject("empty component")),
                "." => {}
                ".." => return Err(reject("parent component")),
                other => parts.push(other),
            }
        }
        if parts.is_empty() {
            return Err(reject("empty path"));
        }
        Ok(Self(parts.join("/")))
    }

    /// The path with `/` separators.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path joined onto `root` with platform separators.
    pub fn to_path(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        path.extend(self.0.split('/'));
        path
    }

    /// Each proper ancestor, from the top-level component down.
    pub fn ancestors(&self) -> Vec<Self> {
        let mut ancestors = Vec::new();
        for (offset, _) in self.0.match_indices('/') {
            if let Some(prefix) = self.0.get(..offset) {
                ancestors.push(Self(prefix.to_owned()));
            }
        }
        ancestors
    }

    /// The final component.
    pub fn file_name(&self) -> &str {
        self.0
            .rsplit_once('/')
            .map_or(self.0.as_str(), |(_, name)| name)
    }
}

impl TryFrom<String> for RelPath {
    type Error = Error;

    fn try_from(raw: String) -> Result<Self> {
        Self::new(&raw)
    }
}

impl From<RelPath> for String {
    fn from(path: RelPath) -> Self {
        path.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::RelPath;

    #[test]
    fn normalizes_current_directory_components() {
        assert_eq!(
            RelPath::new("./mods/./a.pak").unwrap().as_str(),
            "mods/a.pak"
        );
    }

    #[test]
    fn rejects_paths_that_escape_or_differ_by_platform() {
        for bad in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            "a\\b",
            "C:/x",
            "a:stream",
            "a//b",
            "a/",
            ".",
            "a\0b",
        ] {
            assert!(RelPath::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn lists_ancestors_top_down() {
        let path = RelPath::new("a/b/c.txt").unwrap();
        let ancestors: Vec<String> = path
            .ancestors()
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        assert_eq!(ancestors, ["a", "a/b"]);
        assert!(RelPath::new("top.txt").unwrap().ancestors().is_empty());
        assert_eq!(path.file_name(), "c.txt");
    }

    #[test]
    fn validates_when_deserialized() {
        assert!(serde_json::from_str::<RelPath>("\"a/../b\"").is_err());
        let path: RelPath = serde_json::from_str("\"a/b\"").unwrap();
        assert_eq!(serde_json::to_string(&path).unwrap(), "\"a/b\"");
    }
}
