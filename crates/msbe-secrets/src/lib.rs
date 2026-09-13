//! Provider credentials, terms acknowledgements, and the redaction filter.
//!
//! This is the secrets module of MSBE's trusted set (`docs/11-security.md` §11.1). Secret values
//! live only in this crate's types, inside the daemon, and never reach a client.
//!
//! - [`Secret`] zeroizes its value when dropped, never prints it, and registers it with
//!   [`redact`], which the daemon runs over everything it writes.
//! - [`Credentials`] finds a provider's credential in its `MSBE_<PROVIDER>_TOKEN` environment
//!   variable, then in the platform keyring or the passphrase-encrypted file.
//! - [`Access`] is what the provider policy gate sees: whether a provider has a credential and
//!   which terms were acknowledged, with no secret in it.
//!
//! ```text
//! <home>/auth/                  readable only by its owner
//!   credentials.toml            which providers have a credential, where, and when it was used
//!   acknowledgements.toml       the terms acknowledged for each provider
//!   secrets.toml                the encrypted file store
//! ```
//!
//! See `docs/07-browser-and-secrets.md` §7.5.

mod access;
mod acknowledgements;
mod clock;
mod credentials;
mod encrypted;
mod environment;
mod files;
mod os_keyring;
pub mod redact;
mod secret;
mod store;

pub use access::Access;
pub use acknowledgements::{Acknowledgement, Acknowledgements};
pub use clock::{Clock, SystemClock};
pub use credentials::{CredentialRecord, Credentials};
pub use encrypted::{EncryptedFileStore, KdfParams};
pub use environment::EnvironmentStore;
pub use files::auth_directory;
pub use os_keyring::{KEYRING_SERVICE, KeyringStore};
pub use secret::{MAX_SECRET_LEN, MIN_SECRET_LEN, Secret, SecretError};
pub use store::{Backend, MemoryStore, SecretStore, StoreError};
