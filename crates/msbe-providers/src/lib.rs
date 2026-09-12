//! The providers MSBE ships, behind the policy gate.
//!
//! Every provider is its own adapter crate built against `msbe-provider-api`. [`BUILTIN`] lists
//! their registrations, and it is the one place outside those crates that names a provider.
//! [`Providers`] validates their manifests, loads the overlay entries the adapters ship, and
//! refuses a provider whose declared policy this release cannot honour before its adapter runs.
//!
//! See `docs/06-providers-and-policy.md`.

mod registry;
mod runtime;

pub use registry::{BUILTIN, ProgramTrust, Providers, RegistryError, Routed};
