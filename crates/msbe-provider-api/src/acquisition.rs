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
    /// SHA-256 of the transferred bytes, as lowercase hex.
    pub sha256: String,
    /// SHA-512 of the transferred bytes, as lowercase hex.
    pub sha512: String,
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
    verify(
        "SHA-256",
        &descriptor.file_name,
        descriptor.sha256.as_deref(),
        &artifact.sha256,
    )?;
    verify(
        "SHA-512",
        &descriptor.file_name,
        descriptor.sha512.as_deref(),
        &artifact.sha512,
    )?;
    Ok(AcquiredArtifact {
        path,
        size: artifact.bytes,
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
