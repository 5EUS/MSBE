//! Verified acquisition of direct provider artifacts.
//!
//! Adapters supply inert [`ArtifactDescriptor`] values. This module owns the security-sensitive
//! transfer rules so every reviewed adapter gets the same HTTPS, safe-name, size, digest, and
//! create-new guarantees.

use std::{
    io,
    path::{Path, PathBuf},
};

use msbe_fsops::RelPath;
use thiserror::Error;

use crate::{
    HttpClient, HttpError,
    artifact::{ArtifactError, download},
};

/// The most bytes an artifact may be when its provider publishes no size.
pub const DOWNLOAD_LIMIT: u64 = 2 << 30;

/// A provider-published artifact that can be acquired without interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDescriptor {
    /// The HTTPS URL for the exact bytes.
    pub url: String,
    /// The single file name under which to save the artifact.
    pub file_name: String,
    /// The maximum permitted transfer size.
    pub limit: u64,
    /// The exact byte length when the provider publishes it.
    pub size: Option<u64>,
    /// An MD5 digest when the provider publishes one.
    pub md5: Option<String>,
    /// A SHA-1 digest when the provider publishes one.
    pub sha1: Option<String>,
    /// A SHA-256 digest when the provider publishes one.
    pub sha256: Option<String>,
    /// A SHA-512 digest when the provider publishes one.
    pub sha512: Option<String>,
}

/// The verified local result of an artifact acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquiredArtifact {
    /// The newly created file.
    pub path: PathBuf,
    /// The number of transferred bytes.
    pub size: u64,
    /// MD5 of the transferred bytes, as lowercase hex. Only for matching a catalog's lookups.
    pub md5: String,
    /// SHA-1 of the transferred bytes, as lowercase hex. Only for matching a catalog's lookups.
    pub sha1: String,
    /// SHA-256 of the transferred bytes, as lowercase hex.
    pub sha256: String,
    /// SHA-512 of the transferred bytes, as lowercase hex.
    pub sha512: String,
}

impl AcquiredArtifact {
    /// The digest computed with `algorithm`, named as provenance records name it: `md5`, `sha1`,
    /// `sha256` or `sha512`.
    pub fn digest(&self, algorithm: &str) -> Option<&str> {
        match algorithm {
            "md5" => Some(&self.md5),
            "sha1" => Some(&self.sha1),
            "sha256" => Some(&self.sha256),
            "sha512" => Some(&self.sha512),
            _ => None,
        }
    }
}

/// Acquires `descriptor` into `directory` and verifies every published integrity value.
///
/// # Errors
///
/// Returns [`AcquisitionError`] for an insecure URL or file name, a transfer failure, or a
/// published size or digest mismatch. The caller owns `directory` and discards failed files.
pub fn acquire(
    http: &dyn HttpClient,
    descriptor: &ArtifactDescriptor,
    directory: &Path,
) -> Result<AcquiredArtifact, AcquisitionError> {
    if !descriptor.url.starts_with("https://") {
        return Err(AcquisitionError::InsecureUrl(descriptor.url.clone()));
    }
    let name = RelPath::new(&descriptor.file_name)
        .ok()
        .filter(|name| !name.as_str().contains('/'))
        .ok_or_else(|| AcquisitionError::UnsafeFileName(descriptor.file_name.clone()))?;
    let path = directory.join(name.as_str());
    let artifact = download(http, &descriptor.url, &path, descriptor.limit)?;
    if let Some(expected) = descriptor.size
        && artifact.bytes != expected
    {
        return Err(AcquisitionError::SizeMismatch {
            file: descriptor.file_name.clone(),
            expected,
            actual: artifact.bytes,
        });
    }
    for (algorithm, expected, actual) in [
        ("SHA-512", &descriptor.sha512, &artifact.sha512),
        ("SHA-256", &descriptor.sha256, &artifact.sha256),
        ("SHA-1", &descriptor.sha1, &artifact.sha1),
        ("MD5", &descriptor.md5, &artifact.md5),
    ] {
        verify(
            algorithm,
            &descriptor.file_name,
            expected.as_deref(),
            actual,
        )?;
    }
    Ok(AcquiredArtifact {
        path,
        size: artifact.bytes,
        md5: artifact.md5,
        sha1: artifact.sha1,
        sha256: artifact.sha256,
        sha512: artifact.sha512,
    })
}

fn verify(
    algorithm: &'static str,
    file: &str,
    expected: Option<&str>,
    actual: &str,
) -> Result<(), AcquisitionError> {
    if expected.is_some_and(|expected| !expected.eq_ignore_ascii_case(actual)) {
        return Err(AcquisitionError::HashMismatch {
            file: file.to_owned(),
            algorithm,
            expected: expected.unwrap_or_default().to_owned(),
            actual: actual.to_owned(),
        });
    }
    Ok(())
}

/// Why a direct artifact could not be acquired and verified.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AcquisitionError {
    /// The transfer failed.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// The artifact could not be written.
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        /// The destination.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The URL is not `https`.
    #[error("refusing to download over an insecure URL: {0}")]
    InsecureUrl(String),
    /// The file name is not a single safe path component.
    #[error("refusing to write a download named {0:?}")]
    UnsafeFileName(String),
    /// The artifact is not the size its provider published.
    #[error("{file} is {actual} bytes, but the provider published {expected}")]
    SizeMismatch {
        /// The file name.
        file: String,
        /// The published size.
        expected: u64,
        /// The transferred size.
        actual: u64,
    },
    /// The artifact does not match a digest its provider published.
    #[error("{file} failed {algorithm} verification: expected {expected}, got {actual}")]
    HashMismatch {
        /// The file name.
        file: String,
        /// The digest algorithm.
        algorithm: &'static str,
        /// The published digest.
        expected: String,
        /// The digest of what was transferred.
        actual: String,
    },
}

impl From<ArtifactError> for AcquisitionError {
    fn from(error: ArtifactError) -> Self {
        match error {
            ArtifactError::Http(error) => Self::Http(error),
            ArtifactError::Io { path, source } => Self::Io { path, source },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use md5::Md5;
    use sha1::{Digest as _, Sha1};

    use super::{AcquisitionError, ArtifactDescriptor, acquire};
    use crate::{HttpClient, HttpError, HttpRequest, HttpResponse, hashing::hex};

    struct Serves(&'static [u8]);

    impl HttpClient for Serves {
        fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
            Err(HttpError::Status {
                url: request.url.to_owned(),
                status: 404,
            })
        }

        fn download(&self, _: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError> {
            sink.write_all(self.0).unwrap();
            Ok(u64::try_from(self.0.len()).unwrap())
        }
    }

    fn descriptor(name: &str) -> ArtifactDescriptor {
        ArtifactDescriptor {
            url: "https://example.test/mod.zip".to_owned(),
            file_name: name.to_owned(),
            limit: 1024,
            size: None,
            md5: None,
            sha1: None,
            sha256: None,
            sha512: None,
        }
    }

    #[test]
    fn published_sha1_and_md5_digests_are_verified() {
        let dir = tempfile::tempdir().unwrap();
        let mut published = descriptor("a.zip");
        published.sha1 = Some(hex(&Sha1::digest(b"payload")).to_ascii_uppercase());
        published.md5 = Some(hex(&Md5::digest(b"payload")));
        let acquired = acquire(&Serves(b"payload"), &published, dir.path()).unwrap();
        assert_eq!(
            acquired.digest("sha1"),
            published
                .sha1
                .map(|hash| hash.to_ascii_lowercase())
                .as_deref()
        );
        assert_eq!(acquired.digest("md5"), published.md5.as_deref());

        for (name, algorithm) in [("b.zip", "SHA-1"), ("c.zip", "MD5")] {
            let mut wrong = descriptor(name);
            if algorithm == "MD5" {
                wrong.md5 = Some(hex(&Md5::digest(b"other")));
            } else {
                wrong.sha1 = Some(hex(&Sha1::digest(b"other")));
            }
            assert!(matches!(
                acquire(&Serves(b"payload"), &wrong, dir.path()),
                Err(AcquisitionError::HashMismatch { algorithm: found, .. }) if found == algorithm
            ));
        }
    }
}
