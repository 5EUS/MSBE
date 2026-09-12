//! The contract every provider adapter builds against.
//!
//! A provider is a reviewed adapter crate, `msbe-provider-<id>`, that implements [`Adapter`] and
//! exports a [`Registration`]. This crate holds what adapters share and nothing specific to any
//! one of them: neutral project and release records, the HTTP surface, bounded metadata
//! endpoints, verified acquisition, manifests, the overlay, and dependency resolution across any
//! set of adapters. `msbe-providers` lists the registrations MSBE ships.
//!
//! Providers are technically uniform and legally not. Policy is data on each provider's
//! manifest, not scattered conditionals.
//!
//! Adapters reach the network only through [`HttpClient`]. The real implementation lives in
//! `msbe-http`, the one crate that links TLS; everything here is plain Rust that tests drive
//! with in-memory fakes.
//!
//! See `docs/06-providers-and-policy.md`.

mod acquisition;
mod adapter;
mod artifact;
mod codec;
mod endpoint;
mod hashing;
mod http;
pub mod manifest;
pub mod model;
pub mod overlay;
pub mod resolve;
mod target;

pub use acquisition::{
    AcquiredArtifact, AcquisitionError, ArtifactDescriptor, DOWNLOAD_LIMIT, acquire,
};
pub use adapter::{
    Adapter, AdapterError, Build, Registration, Releases, Search, Update, UpdateCheck, Updates,
};
pub use codec::{
    BlobReader, BlobSource, BuildPackCodec, DistributionDecision, EmbeddedBlob, ImportedTarget,
    PackChoice, PackCodec, PackCodecDescriptor, PackCodecError, PackCodecRegistration,
    PackDirections, PackExportContext, PackExportPlan, PackExportResult, PackFile, PackFileRole,
    PackImportContext, PackImportPlan, PackOptionConstraint, PackOptionField, PackOptionKind,
    PackOptionSchema, PackOptionValue, PackOptions, PackPreset, PackProbe, PackRequirement,
    PackWarning, PathMode, ReadSeek, SupportSet, WriteSeek,
};
pub use endpoint::{EndpointError, JsonEndpoint};
pub use hashing::hex;
pub use http::{HttpClient, HttpError};
pub use manifest::{Catalog, ManifestError, Provider, Source};
pub use msbe_core::{instance::Provenance, solver::PackageId};
pub use overlay::{Overlay, OverlayError};
pub use target::{Availability, Target};
