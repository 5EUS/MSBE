//! The providers MSBE ships, behind the policy gate.
//!
//! Every provider is its own adapter crate built against `msbe-provider-api`. [`BUILTIN`] lists
//! their registrations, and it is the one place outside those crates that names a provider.
//! [`Providers`] validates their manifests, loads the overlay entries the adapters ship, and
//! refuses a provider whose declared policy this release cannot honour before its adapter runs.
//!
//! See `docs/06-providers-and-policy.md`.

mod authoring;
#[cfg(test)]
mod conformance_tests;
mod installed;
mod registry;
mod runtime;

pub use authoring::{
    AuthoringError, SignedCodec, SignerKey, VerifiedCodec, sign_codec, verify_codec,
};
pub use installed::{ExtensionTrust, codecs_directory, trust_file};
pub use registry::{BUILTIN, ProgramTrust, Providers, RegistryError, Routed};
