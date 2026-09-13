//! The providers MSBE ships, behind the policy gate.
//!
//! A provider is a declarative program interpreted by a reviewed runtime, or, as a reviewed
//! exception, a native adapter crate built against `msbe-provider-api`. [`BUILTIN_PROGRAMS`] and
//! [`BUILTIN`] list what this build ships, and are the one place outside those crates that names a
//! provider. [`Providers`] validates manifests and programs, loads the overlay entries they ship,
//! and refuses a provider whose declared policy this release cannot honour before its adapter runs.
//!
//! See `docs/06-providers-and-policy.md`.

mod authoring;
#[cfg(test)]
mod catalog_tests;
#[cfg(test)]
mod conformance_tests;
mod installed;
mod registry;
mod runtime;

pub use authoring::{
    AuthoringError, SignedCodec, SignerKey, VerifiedCodec, sign_codec, verify_codec,
};
pub use installed::{ExtensionTrust, codecs_directory, trust_file};
pub use registry::{BUILTIN, BUILTIN_PROGRAMS, ProgramTrust, Providers, RegistryError, Routed};
