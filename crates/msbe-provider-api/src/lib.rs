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
pub mod adapter;
mod artifact;
mod codec;
#[cfg(feature = "conformance")]
pub mod conformance;
mod endpoint;
pub mod envelope;
mod hashing;
mod http;
pub mod manifest;
pub mod model;
pub mod overlay;
pub mod program;
pub mod resolve;
mod target;

/// The host API implemented by this MSBE build for reviewed native extensions.
pub const NATIVE_HOST_API_VERSION: u32 = 1;

pub use acquisition::{
    AcquiredArtifact, AcquisitionError, ArtifactDescriptor, DOWNLOAD_LIMIT, acquire,
};
pub use adapter::{
    Adapter, AdapterError, Build, ProgramRegistration, Registration, Releases, Search, Update,
    UpdateCheck, Updates,
};
pub use codec::{
    BlobSource, BuildPackCodec, ContainerKind, DistributionDecision, EmbeddedBlob, EntryContent,
    EnvironmentRequirement, ImportedTarget, LayoutEntry, Observation, Observations, PackChoice,
    PackCodec, PackCodecDescriptor, PackCodecError, PackCodecRegistration, PackDirections,
    PackEntry, PackExportContext, PackExportPlan, PackFile, PackFileRole, PackImportContext,
    PackImportPlan, PackInclusion, PackInput, PackLayout, PackOptionConstraint, PackOptionField,
    PackOptionKind, PackOptionSchema, PackOptionValue, PackOptions, PackOrigin, PackPreset,
    PackProbe, PackRequirement, PackWarning, PathMode, RequirementSource, SupportSet,
    WasmPackCodecRegistration,
};
pub use ed25519_dalek::{SigningKey, VerifyingKey};
pub use endpoint::{EndpointError, JsonEndpoint};
pub use envelope::{
    EnvelopeError, ExtensionCapability, ExtensionEnvelope, ExtensionProvide, HostApiRange,
};
pub use hashing::hex;
pub use http::{HttpClient, HttpError};
pub use manifest::{Catalog, ManifestError, Provider, Source};
pub use msbe_core::instance::NativeExtensionIdentity;
pub use msbe_core::{instance::Provenance, solver::PackageId};
pub use overlay::{Overlay, OverlayError};
pub use program::{
    Capability, ChannelMapping, Condition, DependencyKinds, DependencyMapping, EachSelector,
    EditionGameIds, Encoding, Facets, FileMapping, GameId, HashAlgorithm, Items, Mappings,
    ObjectMapping, Pages, ProgramError, ProviderProgram, ProviderProgramEnvelope, QueryParameter,
    ReleaseMapping, ReleaseOrder, ReleasesRequest, Routes, RuntimeKind, SearchRequest,
    SegmentedReference, Selector, SingleItem, TargetFact, TextDependency, Translations,
    UpdateFields, UpdateProtocol,
};
pub use target::{Availability, Target};
