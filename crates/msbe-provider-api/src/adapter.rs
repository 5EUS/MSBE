//! The trait a provider adapter implements, and how an adapter registers.

use std::{collections::BTreeMap, error::Error as StdError, fmt, path::Path};

use msbe_core::{instance::Provenance, solver::PackageId};
use thiserror::Error;

use crate::{
    AcquiredArtifact, AcquisitionError, ApiHeaders, ArtifactDescriptor, DOWNLOAD_LIMIT,
    EndpointError, HttpClient, HttpError, ManifestError, PackCodecRegistration, Provider, Target,
    WasmPackCodecRegistration, acquire,
    model::{
        Account, Download, HandoffTicket, Project, Release, ReleaseFile, Request, SearchResult,
    },
};

/// Builds an adapter from its validated manifest.
pub type Build = fn(&Provider) -> Result<Box<dyn Adapter>, ManifestError>;

/// How a reviewed adapter joins MSBE: its manifest, the overlay entries it ships, and its
/// constructor.
///
/// Every adapter crate exports one, and `msbe-providers` lists them. That list is the only place
/// outside an adapter's own crate that names its provider.
#[derive(Debug, Clone, Copy)]
pub struct Registration {
    /// The provider id its manifest declares.
    pub id: &'static str,
    /// Identity metadata for this compiled provider extension.
    pub identity: msbe_core::instance::NativeExtensionIdentity,
    /// The provider manifest, as TOML.
    pub manifest: &'static str,
    /// Overlay entries about the provider's projects, one TOML document each.
    pub overlay: &'static [&'static str],
    /// Builds the adapter from its validated manifest.
    pub build: Build,
    /// Pack formats implemented by this reviewed extension.
    pub pack_codecs: &'static [PackCodecRegistration],
    /// Pack formats this extension ships as sandboxed WebAssembly modules.
    pub wasm_pack_codecs: &'static [WasmPackCodecRegistration],
    /// Why this provider requires reviewed native protocol code instead of a program runtime.
    pub exception_reason: &'static str,
}

/// A provider MSBE ships as a declarative program, with the data that travels with it.
///
/// A shipped program is interpreted by the same reviewed runtime as a signed one. It is trusted as
/// part of the build that embeds it, and pinned like a native extension, by its program's digest.
#[derive(Debug, Clone, Copy)]
pub struct ProgramRegistration {
    /// The provider program, as TOML.
    pub program: &'static str,
    /// The version the program is pinned at.
    pub version: &'static str,
    /// Overlay entries about the provider's projects, one TOML document each.
    pub overlay: &'static [&'static str],
    /// Pack formats the provider ships as sandboxed WebAssembly modules.
    pub wasm_pack_codecs: &'static [WasmPackCodecRegistration],
}

/// A reviewed provider adapter.
///
/// Every provider can parse what a user typed and fetch a file it selected. Everything else is a
/// capability an adapter opts into by returning itself from the matching `as_` method, so a
/// provider without search or updates says so up front rather than failing mid-command. Wire
/// formats stay inside the adapter's crate; what crosses this trait is [`crate::model`].
pub trait Adapter: fmt::Debug {
    /// The provider id this adapter serves.
    fn id(&self) -> &str;

    /// Parses a source reference, with the manifest's source prefix already removed.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] when the reference is not one the provider accepts.
    fn request(&self, reference: &str) -> Result<Request, AdapterError>;

    /// Project search, when the provider supports it.
    fn as_search(&self) -> Option<&dyn Search> {
        None
    }

    /// Project and release metadata for dependency resolution, when the provider publishes it.
    fn as_releases(&self) -> Option<&dyn Releases> {
        None
    }

    /// Update discovery for installed releases, when the provider supports it.
    fn as_updates(&self) -> Option<&dyn Updates> {
        None
    }

    /// Reading and redeeming the links the provider's pages hand mod managers, when it has them.
    fn as_handoff(&self) -> Option<&dyn Handoff> {
        None
    }

    /// Checking which account a credential belongs to, when the provider accepts one.
    fn as_accounts(&self) -> Option<&dyn Accounts> {
        None
    }

    /// The headers the provider's metadata API uses: the one its credential goes in, and the ones
    /// that report remaining quota. The adapter names them and never holds the credential; the
    /// registry attaches it to requests for the provider's metadata origin only.
    fn api_headers(&self) -> ApiHeaders {
        ApiHeaders::default()
    }

    /// Downloads `file` into `dir`, verifying every size and digest the provider published.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::ActionRequired`] for a file MSBE may not download itself, and
    /// otherwise [`AdapterError`] for an insecure URL or unsafe file name, a transfer failure, or a
    /// mismatch. A file that fails verification is left in `dir`, which the caller discards.
    fn acquire(
        &self,
        http: &dyn HttpClient,
        file: &ReleaseFile,
        dir: &Path,
    ) -> Result<AcquiredArtifact, AdapterError> {
        let Download::Direct { url } = &file.download else {
            return Err(AdapterError::ActionRequired {
                file: file.name.clone(),
                download: Box::new(file.download.clone()),
            });
        };
        let descriptor = ArtifactDescriptor {
            url: url.clone(),
            file_name: file.name.clone(),
            limit: file.size.or(file.limit).unwrap_or(DOWNLOAD_LIMIT),
            size: file.size,
            md5: file.md5.clone(),
            sha1: file.sha1.clone(),
            sha256: file.sha256.clone(),
            sha512: file.sha512.clone(),
        };
        Ok(acquire(http, &descriptor, dir)?)
    }

    /// The provenance to record for `release` once one of its files was acquired as `acquired`.
    fn provenance(&self, release: &Release, acquired: &AcquiredArtifact) -> Provenance {
        Provenance {
            provider: release.project.provider.clone(),
            project: release.project.project.clone(),
            version: release.id.clone(),
            version_number: release.number.clone(),
            hashes: BTreeMap::from([
                ("sha256".to_owned(), acquired.sha256.clone()),
                ("sha512".to_owned(), acquired.sha512.clone()),
            ]),
        }
    }
}

/// Searching a provider for projects.
pub trait Search {
    /// Finds at most `limit` projects matching `query` that are compatible with `target`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] for a request or decoding failure.
    fn search(
        &self,
        http: &dyn HttpClient,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, AdapterError>;
}

/// The project and release metadata dependency resolution walks.
///
/// Every operation takes the target, because a catalog that serves several games may address a
/// project only within one of them.
pub trait Releases {
    /// Fetches a project of `target`'s game by any reference the provider accepts, such as a slug
    /// or an id.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] for a malformed reference, a game the provider does not serve, a
    /// request failure, or bad metadata.
    fn project(
        &self,
        http: &dyn HttpClient,
        reference: &str,
        target: &Target,
    ) -> Result<Project, AdapterError>;

    /// The releases of `project`, by its stable id, that are compatible with `target`, the most
    /// preferred first.
    ///
    /// # Errors
    ///
    /// As for [`Releases::project`].
    fn releases(
        &self,
        http: &dyn HttpClient,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Release>, AdapterError>;

    /// The project that owns `release`, for a dependency that names only a release.
    ///
    /// # Errors
    ///
    /// As for [`Releases::project`].
    fn release_project(
        &self,
        http: &dyn HttpClient,
        release: &str,
        target: &Target,
    ) -> Result<PackageId, AdapterError>;
}

/// Finding replacements for releases already installed.
pub trait Updates {
    /// Checks each of `installed`, answering in the same order, for a release to replace it.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] for a request or decoding failure, or a replacement without files.
    fn check(
        &self,
        http: &dyn HttpClient,
        installed: &[&Provenance],
        target: &Target,
    ) -> Result<Vec<UpdateCheck>, AdapterError>;
}

/// Reading and redeeming the links a browser-assisted provider's pages hand mod managers
/// (`docs/06-providers-and-policy.md` §6.6).
pub trait Handoff {
    /// The URI scheme of the links, lowercase and without `://`.
    fn scheme(&self) -> &str;

    /// Reads `uri` against the provider's declared link structure, at `now` in Unix seconds.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] for a link that is too long or of another scheme, names a game the
    /// provider does not serve, has a path or reference that does not fit, or has expired.
    fn parse(&self, uri: &str, now: u64) -> Result<HandoffTicket, AdapterError>;

    /// Redeems `ticket` for the file it names, which MSBE downloads directly. The provider
    /// publishes no digest to verify it against, so its SHA-256 and SHA-512 are recorded as
    /// provenance.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] for another provider's ticket, a request failure, or an answer
    /// without a usable download URL.
    fn redeem(
        &self,
        http: &dyn HttpClient,
        ticket: &HandoffTicket,
    ) -> Result<ReleaseFile, AdapterError>;
}

/// Checking which account a credential belongs to.
pub trait Accounts {
    /// The account that the credential the registry attaches belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] when the provider refuses the credential, or its answer names no
    /// account.
    fn account(&self, http: &dyn HttpClient) -> Result<Account, AdapterError>;
}

/// What updating one installed release would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// The provider does not list the installed file, so there is nothing to compare it with.
    Unlisted,
    /// The installed release is the one to keep.
    Current,
    /// The installed release does not support the target, and nothing that could replace it does.
    Incompatible,
    /// Another release should replace the installed one.
    Available(Box<Update>),
}

/// A release to move to, and the file to install from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The release to move to.
    pub release: Release,
    /// The file to install from it.
    pub file: ReleaseFile,
}

/// Why an adapter operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AdapterError {
    /// A metadata request failed.
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    /// A file could not be acquired and verified.
    #[error(transparent)]
    Acquisition(#[from] AcquisitionError),
    /// MSBE may not download a file itself; the user has to.
    #[error("{file} cannot be downloaded by MSBE: {download}")]
    ActionRequired {
        /// The file's name.
        file: String,
        /// How the user can obtain it.
        download: Box<Download>,
    },
    /// A release has no files to install.
    #[error("release {release} of {project} has no files")]
    NoFiles {
        /// The project the release belongs to.
        project: PackageId,
        /// The release id.
        release: String,
    },
    /// A failure only this provider can describe, such as a malformed reference.
    #[error("{0}")]
    Specific(Box<dyn StdError + Send + Sync>),
}

impl AdapterError {
    /// Wraps a failure specific to one provider.
    pub fn specific(error: impl StdError + Send + Sync + 'static) -> Self {
        Self::Specific(Box::new(error))
    }

    /// Whether the provider reported that what was asked for does not exist.
    pub const fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Endpoint(EndpointError::Http(HttpError::Status { status: 404, .. }))
        )
    }
}
