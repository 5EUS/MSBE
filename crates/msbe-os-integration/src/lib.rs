//! Registers MSBE as the current user's handler of a link scheme, and gives the scheme back.
//!
//! A provider's pages hand files over as links in a scheme its program names, so every function
//! here takes the scheme as input and the crate names no provider. Registration never takes a
//! scheme over silently: while another application opens its links, [`Handlers::register`]
//! refuses unless told to replace it. What it replaces is recorded inside MSBE's own registration,
//! whatever data directory is in use, so [`Handlers::unregister`] always gives it back.
//!
//! | Platform                        | Registration                                                                                  |
//! | ------------------------------- | --------------------------------------------------------------------------------------------- |
//! | Linux and other freedesktop.org | `msbe-handler.desktop` in `$XDG_DATA_HOME/applications`, the default in the user's `mimeapps.list` |
//! | Windows                         | `HKCU\Software\Classes\<scheme>`, never `HKLM`                                                |
//! | macOS                           | The application bundle's `CFBundleURLTypes`, registered when the app is installed             |
//!
//! See `docs/07-browser-and-secrets.md` §7.4.

use std::{
    fmt, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[cfg(all(unix, not(target_os = "macos")))]
mod freedesktop;
#[cfg(test)]
#[cfg(all(unix, not(target_os = "macos")))]
mod freedesktop_tests;
#[cfg(all(unix, not(target_os = "macos")))]
mod keyfile;
#[cfg(windows)]
mod windows;
#[cfg(test)]
#[cfg(windows)]
mod windows_tests;

/// A link scheme MSBE can open: a lowercase URI scheme that is not a web, file or script scheme.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scheme(String);

impl Scheme {
    /// The longest scheme accepted.
    pub const MAX_LEN: usize = 32;

    /// Reads a scheme, ignoring case and a trailing `://`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidScheme`] unless `text` is a URI scheme of at most
    /// [`Self::MAX_LEN`] characters, and is not one a web page or the system already owns.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let scheme = text
            .strip_suffix("://")
            .unwrap_or(text)
            .to_ascii_lowercase();
        let mut bytes = scheme.bytes();
        let valid = bytes.next().is_some_and(|first| first.is_ascii_lowercase())
            && bytes.all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'+' | b'-' | b'.')
            })
            && scheme.len() <= Self::MAX_LEN
            && !matches!(
                scheme.as_str(),
                "http" | "https" | "file" | "ftp" | "data" | "javascript" | "blob" | "about"
            );
        if valid {
            Ok(Self(scheme))
        } else {
            Err(Error::InvalidScheme(text.to_owned()))
        }
    }

    /// The scheme, lowercase and without `://`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which application opens a scheme's links for the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// None does.
    Nobody,
    /// MSBE does.
    Msbe,
    /// Another application does, named as the platform names it.
    Other(String),
}

/// A scheme's registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The scheme.
    pub scheme: Scheme,
    /// Which application opens its links.
    pub owner: Owner,
    /// Whether MSBE's registration opens links with the program asked about. False when MSBE is not
    /// registered, or its registration names a program that has since moved.
    pub current: bool,
    /// The application MSBE replaced, which unregistering gives the scheme back to.
    pub previous: Option<String>,
}

/// Why a registration could not be read or changed.
#[derive(Debug, Error)]
pub enum Error {
    /// The text is not a scheme MSBE can open.
    #[error("{0:?} is not a link scheme MSBE can open")]
    InvalidScheme(String),
    /// Another application opens the scheme's links, and replacing it was not asked for.
    #[error("{scheme} links open with {owner}")]
    Owned {
        /// The scheme.
        scheme: Scheme,
        /// The application that opens them.
        owner: String,
    },
    /// Links are not registered at run time on this platform.
    #[error("{0}")]
    Unsupported(&'static str),
    /// The program links would open with cannot be named in a registration.
    #[error("cannot open links with {}: {reason}", .path.display())]
    Program {
        /// The program.
        path: PathBuf,
        /// Why not.
        reason: &'static str,
    },
    /// A file could not be read or written.
    #[error("cannot {action} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file.
        path: PathBuf,
        /// The failure.
        #[source]
        source: io::Error,
    },
    /// A file could not be replaced.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[error(transparent)]
    Write(#[from] msbe_fsops::Error),
    /// A registry key could not be read or written.
    #[error("cannot {action} registry key {key}: {message}")]
    Registry {
        /// What was being done.
        action: &'static str,
        /// The key, beneath the current user's hive.
        key: String,
        /// The system's message.
        message: String,
    },
}

/// The current user's link handler registrations.
#[derive(Debug, Clone)]
pub struct Handlers {
    platform: Platform,
}

#[derive(Debug, Clone)]
enum Platform {
    #[cfg(all(unix, not(target_os = "macos")))]
    Freedesktop(freedesktop::Desktop),
    #[cfg(windows)]
    Windows(windows::Classes),
    #[cfg_attr(
        windows,
        expect(dead_code, reason = "Windows always has per-user class keys")
    )]
    Unsupported(&'static str),
}

impl Handlers {
    /// The current user's registrations, where this platform keeps them.
    pub fn discover() -> Self {
        #[cfg(all(unix, not(target_os = "macos")))]
        let platform = msbe_core::config::freedesktop().map_or(
            Platform::Unsupported(
                "cannot find the user's home directory, where link handlers are registered",
            ),
            |directories| Platform::Freedesktop(freedesktop::Desktop::new(directories)),
        );
        #[cfg(windows)]
        let platform = Platform::Windows(windows::Classes::current_user());
        #[cfg(target_os = "macos")]
        let platform = Platform::Unsupported(
            "on macOS the MSBE application bundle declares the link schemes it opens, and macOS registers them when the application is installed",
        );
        #[cfg(not(any(unix, windows)))]
        let platform = Platform::Unsupported("this platform has no link handler registration");
        Self { platform }
    }

    /// Registrations in the freedesktop.org directories given, rather than the environment's.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn freedesktop(directories: msbe_core::config::Freedesktop) -> Self {
        Self {
            platform: Platform::Freedesktop(freedesktop::Desktop::new(directories)),
        }
    }

    /// Registrations in `classes`, a key beneath the current user's hive that stands in for
    /// `Software\Classes`. Nothing beneath the local machine's hive is read.
    #[cfg(windows)]
    pub fn windows_classes(classes: &str) -> Self {
        Self {
            platform: Platform::Windows(windows::Classes::at(classes)),
        }
    }

    /// Reports which application opens `scheme`'s links, and whether MSBE's registration opens
    /// them with `program`.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the registration cannot be read, or this platform has none.
    pub fn status(&self, scheme: &Scheme, program: &Path) -> Result<Status, Error> {
        match &self.platform {
            #[cfg(all(unix, not(target_os = "macos")))]
            Platform::Freedesktop(desktop) => desktop.status(scheme, program),
            #[cfg(windows)]
            Platform::Windows(classes) => classes.status(scheme, program),
            Platform::Unsupported(reason) => {
                let _ = (scheme, program);
                Err(Error::Unsupported(reason))
            }
        }
    }

    /// Makes `program handoff <link>` open `scheme`'s links. While another application opens them,
    /// this is refused unless `replace` is set; that application is then recorded, so
    /// [`Self::unregister`] gives the links back to it. Registering again updates the program and
    /// keeps the record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Owned`] when another application opens the links and `replace` is not
    /// set, [`Error::Program`] when `program` is not an absolute path to a file a registration can
    /// name, and [`Error`] when the registration cannot be changed.
    pub fn register(
        &self,
        scheme: &Scheme,
        program: &Path,
        replace: bool,
    ) -> Result<Status, Error> {
        check_program(program)?;
        match &self.platform {
            #[cfg(all(unix, not(target_os = "macos")))]
            Platform::Freedesktop(desktop) => desktop.register(scheme, program, replace),
            #[cfg(windows)]
            Platform::Windows(classes) => classes.register(scheme, program, replace),
            Platform::Unsupported(reason) => {
                let _ = replace;
                Err(Error::Unsupported(reason))
            }
        }
    }

    /// Stops MSBE opening `scheme`'s links and gives them back to the application it replaced. A
    /// scheme another application has taken since is left with it. Unregistering a scheme MSBE
    /// does not open changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the registration cannot be changed, or this platform has none.
    pub fn unregister(&self, scheme: &Scheme, program: &Path) -> Result<Status, Error> {
        match &self.platform {
            #[cfg(all(unix, not(target_os = "macos")))]
            Platform::Freedesktop(desktop) => desktop.unregister(scheme, program),
            #[cfg(windows)]
            Platform::Windows(classes) => classes.unregister(scheme, program),
            Platform::Unsupported(reason) => Err(Error::Unsupported(reason)),
        }
    }
}

/// The `msbe` command beside the running executable, which links are handed to.
///
/// # Errors
///
/// Returns [`Error::Io`] when the running executable cannot be found.
pub fn handler_program() -> Result<PathBuf, Error> {
    let executable = std::env::current_exe().map_err(|source| Error::Io {
        action: "find",
        path: PathBuf::from("the running executable"),
        source,
    })?;
    Ok(executable.with_file_name(format!("msbe{}", std::env::consts::EXE_SUFFIX)))
}

/// Refuses a program that is not an absolute path to a file, or that a registration cannot name.
fn check_program(program: &Path) -> Result<(), Error> {
    let refuse = |reason| Error::Program {
        path: program.to_path_buf(),
        reason,
    };
    if !program.is_absolute() {
        return Err(refuse("the path is not absolute"));
    }
    let text = program
        .to_str()
        .ok_or_else(|| refuse("the path is not valid UTF-8"))?;
    if text
        .chars()
        .any(|character| character.is_control() || character == '"')
    {
        return Err(refuse("the path contains a quote or control character"));
    }
    if !program.is_file() {
        return Err(refuse("no file is there"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Error, Scheme};

    #[test]
    fn schemes_are_lowercase_uri_schemes_that_no_page_or_system_owns() {
        assert_eq!(Scheme::parse("Handoff").unwrap().as_str(), "handoff");
        assert_eq!(
            Scheme::parse("x-link+v1.2://").unwrap().as_str(),
            "x-link+v1.2"
        );
        for refused in [
            "",
            "1link",
            "a/b",
            "a b",
            "https",
            "javascript",
            "file",
            &"a".repeat(33),
        ] {
            assert!(
                matches!(Scheme::parse(refused), Err(Error::InvalidScheme(_))),
                "{refused:?}"
            );
        }
    }
}
