//! Mod source adapters and the provider policy layer.
//!
//! Providers are technically uniform and legally not. Policy is data on each provider,
//! not scattered conditionals, and "we cannot fetch this for you" is a well-typed
//! outcome rather than an error.
//!
//! Providers reach the network only through [`HttpClient`]. The real implementation lives in
//! `msbe-http`, the one crate that links TLS; everything here is plain Rust that tests drive
//! with in-memory fakes.
//!
//! See `docs/06-providers-and-policy.md`.

mod artifact;
pub mod direct;
pub mod endpoint;
mod hashing;
mod http;
pub mod manifest;
pub mod modrinth;
pub mod registry;

pub use endpoint::{EndpointError, JsonEndpoint};
pub use http::{HttpClient, HttpError};
pub use manifest::{Catalog, ManifestError, Provider, Source};
pub use registry::{DIRECT, MODRINTH, ProviderRegistry, RegistryError, ResolvedSource};
