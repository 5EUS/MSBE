//! Typed, stable pack failures (§17.14).

use std::{io, path::PathBuf};

use msbe_core::instance::InstanceError;
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{AdapterError, HttpError, PackCodecError};
use msbe_providers::RegistryError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The stable code a client branches on. Messages are diagnostics, not contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum IssueCode {
    /// No reviewed codec has the requested ID or recognizes the input.
    UnknownCodec,
    /// More than one codec recognizes the input with the same confidence.
    AmbiguousFormat,
    /// The codec cannot run in the requested direction.
    UnsupportedDirection,
    /// The codec cannot represent the profile's plan or loader.
    UnsupportedTarget,
    /// Options or a preset are invalid.
    InvalidOptions,
    /// A manifest exceeds the host's manifest ceiling.
    ManifestTooLarge,
    /// An input entry path is unsafe.
    UnsafeArchivePath,
    /// A requirement does not name an exact release.
    MissingExactVersion,
    /// A requirement does not pin its bytes.
    MissingDigest,
    /// A required blob is neither present nor obtainable.
    MissingBlob,
    /// Content cannot be reproduced under the selected policy.
    UnreproducibleContent,
    /// Policy prohibits embedding the content.
    DistributionForbidden,
    /// Nothing establishes permission to embed the content.
    DistributionUnknown,
    /// Content must be obtained by the user.
    UserActionRequired,
    /// Bytes do not match their declared digest.
    IntegrityMismatch,
    /// An input exceeds a host limit.
    LimitExceeded,
    /// The recipient's installation differs from the one the lockfile was solved against.
    EnvironmentMismatch,
    /// Re-deriving an output produced different bytes.
    DerivationMismatch,
    /// A user change no longer applies to an updated pack layer.
    LayerConflict,
    /// The plan being executed no longer matches current state.
    StalePlan,
    /// An extension failed trust validation.
    UntrustedExtension,
    /// Content pins an installed extension this installation does not serve.
    MissingExtension,
    /// A codec failed without a more specific code.
    CodecFailure,
    /// The caller cancelled the operation.
    Cancelled,
    /// A host operation such as reading instance state failed.
    HostFailure,
}

/// One reason a pack operation cannot proceed or should be reviewed, named in a preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackIssue {
    /// Stable code.
    pub code: IssueCode,
    /// The affected path, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// The affected digest, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
    /// Human-readable detail.
    pub message: String,
}

impl PackIssue {
    /// An issue without a path or digest.
    pub fn new(code: IssueCode, message: impl Into<String>) -> Self {
        Self {
            code,
            path: None,
            digest: None,
            message: message.into(),
        }
    }
}

/// Why a pack operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PackError {
    /// A preview found blockers; execution refuses to run.
    #[error("{}", describe(.0))]
    Blocked(Vec<PackIssue>),
    /// One typed failure.
    #[error("{}", .0.message)]
    Issue(PackIssue),
    /// The caller cancelled the operation.
    #[error("the pack operation was cancelled")]
    Cancelled,
    /// Instance state could not be read or written.
    #[error(transparent)]
    Instance(#[from] InstanceError),
    /// The provider registry refused a codec or adapter.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// A codec or the host container layer failed.
    #[error(transparent)]
    Codec(#[from] PackCodecError),
    /// A provider adapter failed to acquire content.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// A network client could not be opened.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// The content store failed.
    #[error(transparent)]
    Fs(#[from] msbe_fsops::Error),
    /// An artifact could not be ingested.
    #[error(transparent)]
    Archive(#[from] msbe_archive::ArchiveError),
    /// A host file operation failed.
    #[error("cannot {op} {}: {source}", .path.display())]
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl PackError {
    /// A typed failure with `code` and `message`.
    pub fn issue(code: IssueCode, message: impl Into<String>) -> Self {
        Self::Issue(PackIssue::new(code, message))
    }

    /// The stable code for this failure.
    pub fn code(&self) -> IssueCode {
        match self {
            Self::Blocked(issues) => issues
                .first()
                .map_or(IssueCode::HostFailure, |issue| issue.code),
            Self::Issue(issue) => issue.code,
            Self::Cancelled => IssueCode::Cancelled,
            Self::Registry(error) => registry_code(error),
            Self::Codec(error) => codec_code(error),
            Self::Adapter(AdapterError::ActionRequired { .. }) => IssueCode::UserActionRequired,
            Self::Adapter(_) | Self::Http(_) => IssueCode::MissingBlob,
            Self::Archive(_) => IssueCode::LimitExceeded,
            Self::Instance(_) | Self::Fs(_) | Self::Io { .. } => IssueCode::HostFailure,
        }
    }

    /// Every issue this failure reports.
    pub fn issues(&self) -> Vec<PackIssue> {
        match self {
            Self::Blocked(issues) => issues.clone(),
            Self::Issue(issue) => vec![issue.clone()],
            other => vec![PackIssue::new(other.code(), other.to_string())],
        }
    }
}

/// A closure mapping an I/O error on `path` to [`PackError::Io`].
pub(crate) fn io_error<'a>(
    op: &'static str,
    path: &'a std::path::Path,
) -> impl FnOnce(io::Error) -> PackError + 'a {
    move |source| PackError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

fn describe(issues: &[PackIssue]) -> String {
    match issues {
        [] => "the pack operation is blocked".to_owned(),
        [only] => format!("the pack operation is blocked: {}", only.message),
        [first, rest @ ..] => format!(
            "the pack operation is blocked: {} (and {} more)",
            first.message,
            rest.len()
        ),
    }
}

fn registry_code(error: &RegistryError) -> IssueCode {
    match error {
        RegistryError::UnknownCodec(_) => IssueCode::UnknownCodec,
        RegistryError::AmbiguousCodec(_) => IssueCode::AmbiguousFormat,
        RegistryError::PackCodec(error) => codec_code(error),
        RegistryError::Program(_)
        | RegistryError::RevokedProgram(_)
        | RegistryError::RevokedProgramSigner(_)
        | RegistryError::RevokedCodec(_)
        | RegistryError::RevokedCodecSigner(_)
        | RegistryError::ProgramNotGranted { .. }
        | RegistryError::MisnamedProgram { .. } => IssueCode::UntrustedExtension,
        RegistryError::MissingExtension { .. } => IssueCode::MissingExtension,
        RegistryError::ExtensionPinsDiffer => IssueCode::IntegrityMismatch,
        RegistryError::AuthenticationRequired(_)
        | RegistryError::AcknowledgementRequired { .. } => IssueCode::UserActionRequired,
        _ => IssueCode::HostFailure,
    }
}

fn codec_code(error: &PackCodecError) -> IssueCode {
    match error {
        PackCodecError::InvalidOptions(_) => IssueCode::InvalidOptions,
        PackCodecError::UnsupportedDirection(_) => IssueCode::UnsupportedDirection,
        PackCodecError::UnsupportedTarget { .. } => IssueCode::UnsupportedTarget,
        PackCodecError::FormatMismatch => IssueCode::UnknownCodec,
        PackCodecError::Limit(_) => IssueCode::LimitExceeded,
        PackCodecError::UnsafePath(_) => IssueCode::UnsafeArchivePath,
        PackCodecError::MissingBlob(_) => IssueCode::MissingBlob,
        PackCodecError::Unreproducible(_) => IssueCode::UnreproducibleContent,
        PackCodecError::DistributionForbidden(_) => IssueCode::DistributionForbidden,
        PackCodecError::Io(_) => IssueCode::HostFailure,
        _ => IssueCode::CodecFailure,
    }
}
