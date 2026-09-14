//! The capture channel between the daemon and `msbe-browser`.
//!
//! The browser process has no IPC binding into MSBE (`docs/07-browser-and-secrets.md` §7.2). The
//! daemon starts it with two pipes, and each carries frames: a four-byte big-endian length, then
//! that many bytes holding one JSON message, at most [`FRAME_LIMIT`]. Each direction has a closed
//! set of messages. Anything else, including a field a message does not have, is refused, and the
//! daemon then ends the session. Scripts on a page have no path to either pipe.
//!
//! The daemon starts the browser with a [`Launch`]: the profile and quarantine directories, the
//! origins it may be sent to, and the link schemes it captures. This crate names no provider and
//! links no browser, so the daemon can depend on it.

use std::{
    ffi::OsString,
    fmt,
    io::{self, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

/// The most bytes one frame's message may be.
pub const FRAME_LIMIT: usize = 64 * 1024;

/// The longest URL either side accepts.
pub const URL_LIMIT: usize = 8 * 1024;

/// The longest page title the daemon keeps.
pub const TITLE_LIMIT: usize = 1024;

/// How many characters a quarantine file name has: 32 lowercase hexadecimal digits.
pub const QUARANTINE_NAME_LEN: usize = 32;

/// The longest name shown for a captured download.
const NAME_LIMIT: usize = 255;

/// What the browser tells the daemon.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserMessage {
    /// A page handed over a link in a scheme the browser captures. The browser did not let the
    /// operating system open it.
    CapturedProtocolUrl {
        /// The link. Its query may carry a key, so it is never printed.
        url: String,
    },
    /// A download finished in the quarantine directory.
    CapturedDownload {
        /// The name the page suggested. It is shown, never used as a path.
        suggested_name: String,
        /// The file's name in the quarantine directory, which the browser chose at random.
        quarantine_file: String,
        /// The URL the download came from.
        origin_url: String,
        /// How many bytes were downloaded.
        size: u64,
    },
    /// The page the browser shows changed.
    NavigationState {
        /// The page's URL.
        url: String,
        /// The page's title.
        title: String,
    },
}

impl fmt::Debug for BrowserMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapturedProtocolUrl { url } => formatter
                .debug_struct("CapturedProtocolUrl")
                .field(
                    "scheme",
                    &url.split_once(':').map_or("", |(scheme, _)| scheme),
                )
                .finish_non_exhaustive(),
            Self::CapturedDownload {
                suggested_name,
                quarantine_file,
                size,
                ..
            } => formatter
                .debug_struct("CapturedDownload")
                .field("suggested_name", suggested_name)
                .field("quarantine_file", quarantine_file)
                .field("size", size)
                .finish_non_exhaustive(),
            Self::NavigationState { title, .. } => formatter
                .debug_struct("NavigationState")
                .field("title", title)
                .finish_non_exhaustive(),
        }
    }
}

/// What the daemon tells the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DaemonMessage {
    /// Show this page. Both sides refuse a URL that is not HTTPS on an allowed origin.
    Navigate {
        /// The page.
        url: String,
    },
    /// Close the window and exit.
    Close,
}

/// Why a frame could not be read or written.
#[derive(Debug, Error)]
pub enum ChannelError {
    /// The pipe failed.
    #[error("the browser channel failed: {0}")]
    Io(#[from] io::Error),
    /// A frame is longer than [`FRAME_LIMIT`].
    #[error("a browser channel frame of {0} bytes is larger than 64 KiB")]
    TooLarge(usize),
    /// The pipe closed part-way through a frame.
    #[error("the browser channel closed part-way through a frame")]
    Truncated,
    /// A frame does not hold a message this direction carries. The frame is not repeated, since a
    /// link in it may carry a key.
    #[error("a browser channel frame is not a message the channel carries")]
    Malformed,
}

/// Writes `message` as one frame and flushes it.
///
/// # Errors
///
/// Returns [`ChannelError::TooLarge`] when the message is longer than [`FRAME_LIMIT`], and
/// [`ChannelError::Io`] when the pipe fails.
pub fn write_frame<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<(), ChannelError> {
    let body = serde_json::to_vec(message).map_err(|_| ChannelError::Malformed)?;
    if body.len() > FRAME_LIMIT {
        return Err(ChannelError::TooLarge(body.len()));
    }
    let length = u32::try_from(body.len()).map_err(|_| ChannelError::TooLarge(body.len()))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame, or `None` when the pipe closed between frames. A length over
/// [`FRAME_LIMIT`] is refused before its body is read.
///
/// # Errors
///
/// Returns [`ChannelError`] when the frame is too large, cut short, not a message of type `T`, or
/// the pipe fails.
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<Option<T>, ChannelError> {
    let mut length = [0_u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        let Some(rest) = length.get_mut(filled..) else {
            break;
        };
        match reader.read(rest) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ChannelError::Truncated),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let length =
        usize::try_from(u32::from_be_bytes(length)).map_err(|_| ChannelError::Malformed)?;
    if length > FRAME_LIMIT {
        return Err(ChannelError::TooLarge(length));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            ChannelError::Truncated
        } else {
            ChannelError::Io(error)
        }
    })?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| ChannelError::Malformed)
}

/// An HTTPS origin: `https://` and a lowercase host, with a port only when it is not 443.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Origin(String);

impl Origin {
    /// The origin of `url`, when it is an HTTPS URL with a plain host name: no user information,
    /// backslash, percent-encoding, IP literal, whitespace or control character.
    pub fn of(url: &str) -> Option<Self> {
        if url.len() > URL_LIMIT
            || url.contains('\\')
            || url
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return None;
        }
        let rest = url
            .get(..8)
            .filter(|prefix| prefix.eq_ignore_ascii_case("https://"))
            .and(url.get(8..))?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains(['@', '%', '[', ']']) {
            return None;
        }
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => {
                if port.is_empty()
                    || port.len() > 5
                    || !port.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return None;
                }
                let port: u16 = port.parse().ok().filter(|port| *port != 0)?;
                (host, (port != 443).then_some(port))
            }
            None => (authority, None),
        };
        let valid_host = !host.is_empty()
            && !host.starts_with(['.', '-'])
            && !host.ends_with(['.', '-'])
            && !host.contains("..")
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'));
        if !valid_host {
            return None;
        }
        let host = host.to_ascii_lowercase();
        Some(Self(port.map_or_else(
            || format!("https://{host}"),
            |port| format!("https://{host}:{port}"),
        )))
    }

    /// The origin, as `https://host` or `https://host:port`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Whether the browser may be sent to `url`: HTTPS on one of `origins`.
pub fn is_navigable(url: &str, origins: &[Origin]) -> bool {
    Origin::of(url).is_some_and(|origin| origins.contains(&origin))
}

/// Whether `scheme` can be captured: a lowercase URI scheme that is not a web, file or script
/// scheme, the same rule provider programs follow for their link schemes.
pub fn is_capturable_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')
        })
        && scheme.len() <= 32
        && !matches!(
            scheme,
            "http" | "https" | "file" | "ftp" | "data" | "javascript" | "blob" | "about"
        )
}

/// The scheme of `url`, lowercase, when it is one the browser captures among `schemes`.
pub fn captured_scheme<'a>(url: &str, schemes: &'a [String]) -> Option<&'a str> {
    let (scheme, _) = url.split_once(':')?;
    schemes
        .iter()
        .map(String::as_str)
        .find(|captured| captured.eq_ignore_ascii_case(scheme))
}

/// A random quarantine file name: 32 lowercase hexadecimal digits.
pub fn quarantine_name(random: [u8; 16]) -> String {
    random
        .iter()
        .fold(String::with_capacity(32), |mut name, byte| {
            for nibble in [byte >> 4, byte & 0x0f] {
                name.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
            }
            name
        })
}

/// Whether `name` is a name [`quarantine_name`] makes, so it cannot leave the quarantine directory.
pub fn is_quarantine_name(name: &str) -> bool {
    name.len() == QUARANTINE_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The name to show for a download, from the name a page suggested: its last path segment, with
/// control characters dropped and at most 255 bytes. It is shown, never used as a path.
pub fn display_name(suggested: &str) -> Option<String> {
    let last = suggested.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut name = String::new();
    for character in last.chars().filter(|character| !character.is_control()) {
        if name.len() + character.len_utf8() > NAME_LIMIT {
            break;
        }
        name.push(character);
    }
    let name = name.trim().to_owned();
    (!name.is_empty() && name != "." && name != "..").then_some(name)
}

/// How the daemon starts the browser. Each value is one `--msbe-<name>=<value>` argument, which
/// Chromium ignores, so the browser's own subprocesses can be told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The browser's own profile directory, one for each provider and never the user's.
    pub profile: PathBuf,
    /// Where downloads land, under random names.
    pub quarantine: PathBuf,
    /// The origins the daemon may send the browser to.
    pub origins: Vec<Origin>,
    /// The link schemes the browser captures instead of letting the operating system open them.
    pub schemes: Vec<String>,
}

/// Why the browser's arguments could not be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LaunchError {
    /// A required argument is missing.
    #[error("msbe-browser needs --msbe-{0}")]
    Missing(&'static str),
    /// An argument's value is not valid.
    #[error("--msbe-{0} is not valid")]
    Invalid(&'static str),
}

const PROFILE: &str = "--msbe-profile=";
const QUARANTINE: &str = "--msbe-quarantine=";
const ORIGIN: &str = "--msbe-origin=";
const SCHEME: &str = "--msbe-scheme=";

impl Launch {
    /// The arguments that start the browser with this launch.
    pub fn arguments(&self) -> Vec<OsString> {
        let path = |prefix: &str, path: &PathBuf| {
            let mut argument = OsString::from(prefix);
            argument.push(path);
            argument
        };
        let mut arguments = vec![
            path(PROFILE, &self.profile),
            path(QUARANTINE, &self.quarantine),
        ];
        arguments.extend(
            self.origins
                .iter()
                .map(|origin| OsString::from(format!("{ORIGIN}{origin}"))),
        );
        arguments.extend(
            self.schemes
                .iter()
                .map(|scheme| OsString::from(format!("{SCHEME}{scheme}"))),
        );
        arguments
    }

    /// Reads a launch from the browser's arguments, ignoring every argument that is not one of its
    /// own. The directories must be absolute, each origin must be what [`Origin::of`] makes, and
    /// each scheme capturable.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] when a directory is missing, or a value is not valid.
    pub fn parse<I>(arguments: I) -> Result<Self, LaunchError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut profile = None;
        let mut quarantine = None;
        let mut origins = Vec::new();
        let mut schemes = Vec::new();
        for argument in arguments {
            let Some(text) = argument.to_str() else {
                if argument.to_string_lossy().starts_with("--msbe-") {
                    return Err(LaunchError::Invalid("arguments"));
                }
                continue;
            };
            if let Some(value) = text.strip_prefix(PROFILE) {
                profile = Some(absolute(value, "profile")?);
            } else if let Some(value) = text.strip_prefix(QUARANTINE) {
                quarantine = Some(absolute(value, "quarantine")?);
            } else if let Some(value) = text.strip_prefix(ORIGIN) {
                let origin = Origin::of(value)
                    .filter(|origin| origin.as_str() == value)
                    .ok_or(LaunchError::Invalid("origin"))?;
                origins.push(origin);
            } else if let Some(value) = text.strip_prefix(SCHEME) {
                if !is_capturable_scheme(value) {
                    return Err(LaunchError::Invalid("scheme"));
                }
                schemes.push(value.to_owned());
            } else if text.starts_with("--msbe-") {
                return Err(LaunchError::Invalid("arguments"));
            }
        }
        Ok(Self {
            profile: profile.ok_or(LaunchError::Missing("profile"))?,
            quarantine: quarantine.ok_or(LaunchError::Missing("quarantine"))?,
            origins,
            schemes,
        })
    }
}

fn absolute(value: &str, name: &'static str) -> Result<PathBuf, LaunchError> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(LaunchError::Invalid(name))
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, io::Cursor, path::PathBuf};

    use super::{
        BrowserMessage, ChannelError, DaemonMessage, FRAME_LIMIT, Launch, LaunchError, Origin,
        captured_scheme, display_name, is_navigable, is_quarantine_name, quarantine_name,
        read_frame, write_frame,
    };

    #[test]
    fn frames_round_trip_and_end_cleanly_between_frames() {
        let mut pipe = Vec::new();
        let captured = BrowserMessage::CapturedDownload {
            suggested_name: "gear.zip".to_owned(),
            quarantine_file: "0".repeat(32),
            origin_url: "https://files.example.test/gear.zip".to_owned(),
            size: 12,
        };
        write_frame(&mut pipe, &captured).unwrap();
        write_frame(&mut pipe, &DaemonMessage::Close).unwrap();
        let mut reader = Cursor::new(pipe);
        assert_eq!(
            read_frame::<BrowserMessage>(&mut reader).unwrap(),
            Some(captured)
        );
        assert_eq!(
            read_frame::<DaemonMessage>(&mut reader).unwrap(),
            Some(DaemonMessage::Close)
        );
        assert_eq!(read_frame::<DaemonMessage>(&mut reader).unwrap(), None);
    }

    #[test]
    fn frames_over_the_limit_are_refused_on_both_sides() {
        let huge = DaemonMessage::Navigate {
            url: "a".repeat(FRAME_LIMIT),
        };
        assert!(matches!(
            write_frame(&mut Vec::new(), &huge),
            Err(ChannelError::TooLarge(_))
        ));

        // Only the length is read: the body a reader would have to allocate is never there.
        let length = u32::try_from(FRAME_LIMIT + 1).unwrap().to_be_bytes();
        assert!(matches!(
            read_frame::<BrowserMessage>(&mut Cursor::new(length.to_vec())),
            Err(ChannelError::TooLarge(65_537))
        ));
        let mut cut = 10_u32.to_be_bytes().to_vec();
        cut.extend_from_slice(b"{\"kind\"");
        assert!(matches!(
            read_frame::<BrowserMessage>(&mut Cursor::new(cut)),
            Err(ChannelError::Truncated)
        ));
        assert!(matches!(
            read_frame::<BrowserMessage>(&mut Cursor::new(vec![0, 0])),
            Err(ChannelError::Truncated)
        ));
    }

    #[test]
    fn unknown_messages_and_fields_are_refused() {
        for body in [
            r#"{"kind":"run_script","source":"alert(1)"}"#,
            r#"{"kind":"navigation_state","url":"https://a.test","title":"A","extra":1}"#,
            r#"{"kind":"navigate","url":"https://a.test"}"#,
            r#"{"url":"https://a.test"}"#,
            "not json",
        ] {
            let mut frame = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
            frame.extend_from_slice(body.as_bytes());
            assert!(
                matches!(
                    read_frame::<BrowserMessage>(&mut Cursor::new(frame)),
                    Err(ChannelError::Malformed)
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn navigation_is_only_to_https_on_an_allowed_origin() {
        let allowed = [Origin::of("https://www.example.test/mods").unwrap()];
        assert_eq!(allowed[0].as_str(), "https://www.example.test");
        assert!(is_navigable(
            "https://WWW.example.test:443/mods/1?file=2",
            &allowed
        ));
        for refused in [
            "http://www.example.test/mods",
            "https://evil.test/mods",
            "https://www.example.test.evil.test/",
            "https://user@www.example.test/",
            "https://evil.test\\@www.example.test/",
            "https://www.example.test:8443/",
            "https://www.%65xample.test/",
            "https://[::1]/",
            "javascript:alert(1)",
            "https://www.example.test/a b",
            "file:///etc/passwd",
        ] {
            assert!(!is_navigable(refused, &allowed), "{refused}");
        }
        assert_eq!(
            Origin::of("https://Files.Example.test:8443/x")
                .unwrap()
                .as_str(),
            "https://files.example.test:8443"
        );
    }

    #[test]
    fn quarantine_names_are_random_hex_that_cannot_leave_the_directory() {
        let name = quarantine_name([0xab; 16]);
        assert_eq!(name, "ab".repeat(16));
        assert!(is_quarantine_name(&name));
        for refused in [
            "../../etc/passwd",
            "gear.zip",
            &"A".repeat(32),
            &"a".repeat(31),
            &format!("{}/", "a".repeat(31)),
            "",
        ] {
            assert!(!is_quarantine_name(refused), "{refused}");
        }
    }

    #[test]
    fn suggested_names_are_shown_without_any_path() {
        assert_eq!(display_name("../../gear.zip").as_deref(), Some("gear.zip"));
        assert_eq!(
            display_name("C:\\x\\gear\u{7}.zip").as_deref(),
            Some("gear.zip")
        );
        assert_eq!(display_name(".."), None);
        assert_eq!(display_name("  "), None);
        assert_eq!(
            display_name(&"é".repeat(200)).map(|name| name.len()),
            Some(254)
        );
    }

    #[test]
    fn captured_links_are_never_printed() {
        let message = BrowserMessage::CapturedProtocolUrl {
            url: "handoff://game/files/1/2?key=secret".to_owned(),
        };
        let printed = format!("{message:?}");
        assert!(
            printed.contains("handoff") && !printed.contains("secret"),
            "{printed}"
        );
        let schemes = vec!["handoff".to_owned()];
        assert_eq!(captured_scheme("HANDOFF://x", &schemes), Some("handoff"));
        assert_eq!(captured_scheme("other://x", &schemes), None);
    }

    #[test]
    fn launches_round_trip_through_arguments_among_chromium_switches() {
        let launch = Launch {
            profile: PathBuf::from("/home/user/msbe/browser/example/profile"),
            quarantine: PathBuf::from("/home/user/msbe/browser/example/quarantine"),
            origins: vec![Origin::of("https://www.example.test").unwrap()],
            schemes: vec!["handoff".to_owned()],
        };
        let mut arguments = vec![
            OsString::from("msbe-browser"),
            OsString::from("--type=renderer"),
        ];
        arguments.extend(launch.arguments());
        assert_eq!(Launch::parse(arguments).unwrap(), launch);

        let refused = |argument: &str| {
            Launch::parse([
                OsString::from("--msbe-profile=/p"),
                OsString::from("--msbe-quarantine=/q"),
                OsString::from(argument),
            ])
        };
        assert_eq!(
            refused("--msbe-origin=http://a.test"),
            Err(LaunchError::Invalid("origin"))
        );
        assert_eq!(
            refused("--msbe-origin=https://A.test"),
            Err(LaunchError::Invalid("origin"))
        );
        assert_eq!(
            refused("--msbe-scheme=https"),
            Err(LaunchError::Invalid("scheme"))
        );
        assert_eq!(
            refused("--msbe-debug=1"),
            Err(LaunchError::Invalid("arguments"))
        );
        assert_eq!(
            Launch::parse([OsString::from("--msbe-profile=relative")]),
            Err(LaunchError::Invalid("profile"))
        );
        assert_eq!(
            Launch::parse([OsString::from("--msbe-profile=/p")]),
            Err(LaunchError::Missing("quarantine"))
        );
    }
}
