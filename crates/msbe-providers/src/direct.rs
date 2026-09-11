//! Direct downloads: an `https` URL, optionally pinned to a checksum.
//!
//! The manual escape hatch from `docs/06-providers-and-policy.md`. A direct URL has no
//! metadata to trust, so the URL is the mod's identity. A checksum pinned in the fragment
//! (`#sha256=...` or `#sha512=...`) is verified when present, and the SHA-512 of what was
//! fetched is recorded either way.

use std::{
    io,
    path::{Path, PathBuf},
};

use msbe_fsops::RelPath;
use thiserror::Error;

use crate::{
    acquisition::{AcquisitionError, ArtifactDescriptor, acquire},
    http::{HttpClient, HttpError},
};

/// The most bytes a direct download may be.
pub const DOWNLOAD_LIMIT: u64 = 2 << 30;

/// A checksum pinned in a URL's fragment, as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    /// SHA-256.
    Sha256(String),
    /// SHA-512.
    Sha512(String),
}

/// A direct download, parsed from its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectSource {
    /// The URL to fetch, without its fragment.
    pub url: String,
    /// The name the file is saved under: the last segment of the URL's path, percent-decoded.
    pub file_name: String,
    /// The checksum the download must match, if the URL pins one.
    pub checksum: Option<Checksum>,
}

impl DirectSource {
    /// Parses `https://host/path/file[?query][#sha256=<hex>|#sha512=<hex>]`.
    ///
    /// # Errors
    ///
    /// Returns [`DirectError`] for a URL that is not `https`, does not end in a usable file
    /// name, or pins a malformed checksum.
    pub fn parse(raw: &str) -> Result<Self, DirectError> {
        let (url, fragment) = match raw.split_once('#') {
            Some((url, fragment)) => (url, Some(fragment)),
            None => (raw, None),
        };
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(DirectError::Insecure(raw.to_owned()));
        };
        let without_query = rest.split('?').next().unwrap_or_default();
        let file_name = without_query
            .split_once('/')
            .and_then(|(host, path)| (!host.is_empty()).then_some(path))
            .and_then(|path| path.rsplit('/').next())
            .and_then(percent_decode)
            .filter(|name| {
                !name.starts_with('.') && !name.contains('/') && RelPath::new(name).is_ok()
            })
            .ok_or_else(|| DirectError::NoFileName(raw.to_owned()))?;
        Ok(Self {
            url: url.to_owned(),
            file_name,
            checksum: fragment.map(parse_checksum).transpose()?,
        })
    }
}

/// A finished direct download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// Where the file was saved.
    pub path: PathBuf,
    /// Its SHA-512, as lowercase hex.
    pub sha512: String,
    /// Its size in bytes.
    pub size: u64,
}

/// Downloads `source` into `dir` and verifies its pinned checksum, if it has one.
///
/// A file that fails verification is left in `dir`, which the caller owns and discards.
///
/// # Errors
///
/// Returns [`DirectError`] for a transfer failure, a file that cannot be written, or a
/// checksum mismatch.
pub fn download(
    http: &dyn HttpClient,
    source: &DirectSource,
    dir: &Path,
) -> Result<Downloaded, DirectError> {
    let descriptor = ArtifactDescriptor {
        url: source.url.clone(),
        file_name: source.file_name.clone(),
        limit: DOWNLOAD_LIMIT,
        size: None,
        sha256: match &source.checksum {
            Some(Checksum::Sha256(digest)) => Some(digest.clone()),
            Some(Checksum::Sha512(_)) | None => None,
        },
        sha512: match &source.checksum {
            Some(Checksum::Sha512(digest)) => Some(digest.clone()),
            Some(Checksum::Sha256(_)) | None => None,
        },
    };
    let artifact = acquire(http, &descriptor, dir)?;

    Ok(Downloaded {
        path: artifact.path,
        sha512: artifact.sha512,
        size: artifact.size,
    })
}

/// Decodes `%XX` escapes. Returns `None` for a malformed escape or a result that is not UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let escape = bytes
                .get(index + 1..index + 3)
                .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))?;
            decoded.push(u8::from_str_radix(std::str::from_utf8(escape).ok()?, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn parse_checksum(fragment: &str) -> Result<Checksum, DirectError> {
    let invalid = || DirectError::InvalidChecksum(fragment.to_owned());
    let (algorithm, digest) = fragment.split_once('=').ok_or_else(invalid)?;
    let hex_of_length =
        |length| digest.len() == length && digest.chars().all(|c| c.is_ascii_hexdigit());
    match algorithm {
        "sha256" if hex_of_length(64) => Ok(Checksum::Sha256(digest.to_ascii_lowercase())),
        "sha512" if hex_of_length(128) => Ok(Checksum::Sha512(digest.to_ascii_lowercase())),
        _ => Err(invalid()),
    }
}

/// Why a direct download failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DirectError {
    /// The URL is not `https`.
    #[error("refusing to download over an insecure URL: {0}")]
    Insecure(String),

    /// The URL does not end in a file name.
    #[error("{0} does not end in a file name to save the download as")]
    NoFileName(String),

    /// The fragment is not a checksum this client understands.
    #[error(
        "{0:?} is not a checksum; pin one as #sha256=<64 hex digits> or #sha512=<128 hex digits>"
    )]
    InvalidChecksum(String),

    /// The request failed.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The download does not match its pinned checksum.
    #[error("{file} failed {algorithm} verification: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The file name.
        file: String,
        /// The checksum algorithm.
        algorithm: &'static str,
        /// The pinned checksum.
        expected: String,
        /// The checksum of what was downloaded.
        actual: String,
    },

    /// The download could not be written.
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        /// The destination.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl From<AcquisitionError> for DirectError {
    fn from(error: AcquisitionError) -> Self {
        match error {
            AcquisitionError::Transfer(error) => match error {
                crate::artifact::ArtifactError::Http(error) => Self::Http(error),
                crate::artifact::ArtifactError::Io { path, source } => Self::Io { path, source },
            },
            AcquisitionError::InsecureUrl(url) => Self::Insecure(url),
            AcquisitionError::UnsafeFileName(_) => unreachable!("DirectSource validates names"),
            AcquisitionError::SizeMismatch { .. } => unreachable!("direct URLs publish no size"),
            AcquisitionError::HashMismatch {
                file,
                algorithm,
                expected,
                actual,
            } => Self::ChecksumMismatch {
                file,
                algorithm,
                expected,
                actual,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use sha2::{Digest as _, Sha256, Sha512};

    use super::{Checksum, DirectError, DirectSource, download};
    use crate::{
        hashing::hex,
        http::{HttpClient, HttpError},
    };

    /// Serves the same bytes for every download.
    struct Serves(&'static [u8]);

    impl HttpClient for Serves {
        fn get(&self, url: &str, _: &[(&str, &str)], _: u64) -> Result<Vec<u8>, HttpError> {
            Err(HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
        }

        fn post_json(&self, url: &str, _: &[u8], _: u64) -> Result<Vec<u8>, HttpError> {
            Err(HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
        }

        fn download(
            &self,
            _url: &str,
            sink: &mut dyn Write,
            _limit: u64,
        ) -> Result<u64, HttpError> {
            sink.write_all(self.0).unwrap();
            Ok(u64::try_from(self.0.len()).unwrap())
        }
    }

    #[test]
    fn urls_must_be_https_name_a_file_and_pin_well_formed_checksums() {
        let raw = format!(
            "https://cdn.example/mods/extra-1.0.jar?download=1#sha256={}",
            "AB".repeat(32)
        );
        assert_eq!(
            DirectSource::parse(&raw).unwrap(),
            DirectSource {
                url: "https://cdn.example/mods/extra-1.0.jar?download=1".to_owned(),
                file_name: "extra-1.0.jar".to_owned(),
                checksum: Some(Checksum::Sha256("ab".repeat(32))),
            }
        );
        // Found against the Modrinth CDN, whose file names escape `+`.
        let encoded =
            DirectSource::parse("https://cdn.example/v/sodium-fabric-0.8.13%2Bmc1.21.1.jar")
                .unwrap();
        assert_eq!(encoded.file_name, "sodium-fabric-0.8.13+mc1.21.1.jar");
        assert_eq!(
            encoded.url,
            "https://cdn.example/v/sodium-fabric-0.8.13%2Bmc1.21.1.jar"
        );
        assert!(matches!(
            DirectSource::parse("http://cdn.example/a.jar"),
            Err(DirectError::Insecure(_))
        ));
        for no_name in [
            "https://cdn.example",
            "https://cdn.example/dir/",
            "https://cdn.example/.hidden",
            "https:///a.jar",
            "https://cdn.example/escape%2F..%2Fa.jar",
            "https://cdn.example/%2e%2e",
            "https://cdn.example/bad%zz.jar",
            "https://cdn.example/truncated%2",
        ] {
            assert!(
                matches!(
                    DirectSource::parse(no_name),
                    Err(DirectError::NoFileName(_))
                ),
                "{no_name}"
            );
        }
        let short = format!("https://cdn.example/a.jar#sha256={}", "a".repeat(63));
        for bad in ["https://cdn.example/a.jar#md5=abc", short.as_str()] {
            assert!(
                matches!(
                    DirectSource::parse(bad),
                    Err(DirectError::InvalidChecksum(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_pinned_checksum_is_verified_and_the_sha512_is_always_recorded() {
        let good = format!(
            "https://cdn.example/extra.jar#sha256={}",
            hex(&Sha256::digest(b"payload"))
        );
        let dir = tempfile::tempdir().unwrap();
        let downloaded = download(
            &Serves(b"payload"),
            &DirectSource::parse(&good).unwrap(),
            dir.path(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&downloaded.path).unwrap(), b"payload");
        assert_eq!(downloaded.sha512, hex(&Sha512::digest(b"payload")));
        assert_eq!(downloaded.size, 7);

        let bad = format!("https://cdn.example/extra.jar#sha512={}", "0".repeat(128));
        let other = tempfile::tempdir().unwrap();
        let result = download(
            &Serves(b"payload"),
            &DirectSource::parse(&bad).unwrap(),
            other.path(),
        );
        assert!(
            matches!(
                result,
                Err(DirectError::ChecksumMismatch {
                    algorithm: "SHA-512",
                    ..
                })
            ),
            "{result:?}"
        );
    }
}
