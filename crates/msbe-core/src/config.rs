//! Where MSBE keeps its own state, and the one place the process environment is read.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::instance::{InstanceError, Name};

/// MSBE's data directory: instances, their journals, profiles and pinned plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Uses `root` as the data directory.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Finds the data directory: `$MSBE_HOME` if set, otherwise `msbe` inside the platform's
    /// per-user data directory.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::HomeUnavailable`] if neither can be determined.
    pub fn discover() -> Result<Self, InstanceError> {
        if let Some(explicit) = env("MSBE_HOME") {
            return Ok(Self::at(explicit));
        }
        platform_data_dir()
            .map(|dir| Self::at(dir.join("msbe")))
            .ok_or(InstanceError::HomeUnavailable)
    }

    /// The data directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding every instance.
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances")
    }

    /// The directory holding one instance's state.
    pub fn instance(&self, name: &Name) -> PathBuf {
        self.instances().join(name.as_str())
    }
}

/// Reads one environment variable, treating an empty value as unset.
fn env(key: &str) -> Option<OsString> {
    #[expect(
        clippy::disallowed_methods,
        reason = "configuration is read here and nowhere else in the workspace"
    )]
    let value = std::env::var_os(key);
    value.filter(|value| !value.is_empty())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_data_dir() -> Option<PathBuf> {
    env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".local").join("share")))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> Option<PathBuf> {
    env("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
    })
}

#[cfg(windows)]
fn platform_data_dir() -> Option<PathBuf> {
    env("APPDATA").map(PathBuf::from)
}

#[cfg(not(any(unix, windows)))]
fn platform_data_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::Home;
    use crate::instance::Name;

    #[test]
    fn instance_state_lives_under_the_data_directory() {
        let home = Home::at("/data/msbe");
        let name = Name::new("demo").unwrap();
        assert_eq!(
            home.instances(),
            std::path::Path::new("/data/msbe/instances")
        );
        assert_eq!(
            home.instance(&name),
            std::path::Path::new("/data/msbe/instances/demo")
        );
    }
}
