//! The store contract, and the errors every store shares.

use std::{collections::BTreeMap, fmt, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{Secret, SecretError};

/// The longest provider id a store accepts.
const PROVIDER_LIMIT: usize = 64;

/// Where a credential is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// A `MSBE_<PROVIDER>_TOKEN` environment variable.
    Environment,
    /// The platform keyring.
    Keyring,
    /// The passphrase-encrypted file in the data directory.
    EncryptedFile,
    /// Process memory, which nothing persists.
    Memory,
}

impl fmt::Display for Backend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Environment => "environment",
            Self::Keyring => "keyring",
            Self::EncryptedFile => "encrypted file",
            Self::Memory => "memory",
        })
    }
}

/// A place that keeps at most one secret per provider.
pub trait SecretStore: fmt::Debug + Send {
    /// Which kind of store this is.
    fn backend(&self) -> Backend;

    /// The secret for `provider`, if this store has one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid provider id or a store that cannot be read.
    fn get(&self, provider: &str) -> Result<Option<Secret>, StoreError>;

    /// Keeps `secret` for `provider`, replacing any it had.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid provider id, or a read-only or failing store.
    fn set(&mut self, provider: &str, secret: &Secret) -> Result<(), StoreError>;

    /// Forgets `provider`'s secret, reporting whether there was one.
    ///
    /// # Errors
    ///
    /// As for [`SecretStore::set`].
    fn delete(&mut self, provider: &str) -> Result<bool, StoreError>;
}

/// A store in process memory, for tests and for runs that must not persist a credential.
#[derive(Default)]
pub struct MemoryStore {
    secrets: BTreeMap<String, Zeroizing<String>>,
}

impl fmt::Debug for MemoryStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryStore")
            .field("providers", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SecretStore for MemoryStore {
    fn backend(&self) -> Backend {
        Backend::Memory
    }

    fn get(&self, provider: &str) -> Result<Option<Secret>, StoreError> {
        validate_provider(provider)?;
        Ok(self
            .secrets
            .get(provider)
            .map(|value| Secret::new(value.as_str().to_owned()))
            .transpose()?)
    }

    fn set(&mut self, provider: &str, secret: &Secret) -> Result<(), StoreError> {
        validate_provider(provider)?;
        self.secrets.insert(
            provider.to_owned(),
            Zeroizing::new(secret.expose().to_owned()),
        );
        Ok(())
    }

    fn delete(&mut self, provider: &str) -> Result<bool, StoreError> {
        validate_provider(provider)?;
        Ok(self.secrets.remove(provider).is_some())
    }
}

/// Checks that `provider` is a provider id: lowercase ASCII letters, digits and hyphens.
pub(crate) fn validate_provider(provider: &str) -> Result<(), StoreError> {
    let valid = !provider.is_empty()
        && provider.len() <= PROVIDER_LIMIT
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidProvider(provider.to_owned()))
    }
}

/// Why a credential could not be found, kept or forgotten. No variant carries a secret value.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A provider id is not lowercase ASCII letters, digits and hyphens.
    #[error("{0:?} is not a provider id")]
    InvalidProvider(String),
    /// A value is not acceptable as a secret.
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// A provider's environment variable does not hold an acceptable token.
    #[error("{variable} does not hold a usable token: {error}")]
    InvalidEnvironment {
        /// The variable.
        variable: String,
        /// What is wrong with its value.
        error: SecretError,
    },
    /// The store cannot be written.
    #[error("the {0} store is read-only")]
    ReadOnly(Backend),
    /// The store cannot be reached, such as a keyring without a desktop session.
    #[error("the {backend} store is unavailable: {reason}")]
    Unavailable {
        /// The store.
        backend: Backend,
        /// Why.
        reason: String,
    },
    /// No store can keep a new credential.
    #[error(
        "no store can keep a credential: the keyring is unavailable and the encrypted file is locked"
    )]
    NoWritableStore,
    /// The store failed.
    #[error("the {backend} store failed: {reason}")]
    Failed {
        /// The store.
        backend: Backend,
        /// What went wrong.
        reason: String,
    },
    /// The encrypted file has not been unlocked in this process.
    #[error("the encrypted secrets file is locked; unlock it with its passphrase first")]
    Locked,
    /// The passphrase does not open the encrypted file, or the file was altered.
    #[error("the passphrase does not open {}, or the file was altered", .0.display())]
    WrongPassphrase(PathBuf),
    /// A file is not in the format this release reads.
    #[error("{} is not a valid {kind}: {reason}", .path.display())]
    Malformed {
        /// The file.
        path: PathBuf,
        /// What the file should be.
        kind: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// A file could not be read or written.
    #[error("cannot {action} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A file could not be replaced atomically.
    #[error(transparent)]
    Replace(#[from] msbe_fsops::Error),
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, SecretStore, StoreError, validate_provider};
    use crate::Secret;

    #[test]
    fn provider_ids_are_lowercase_letters_digits_and_hyphens() {
        for valid in ["example", "example-2", "a"] {
            validate_provider(valid).unwrap();
        }
        for invalid in ["", "Example", "a_b", "a/b", "a.b", &"a".repeat(65)] {
            assert!(matches!(
                validate_provider(invalid),
                Err(StoreError::InvalidProvider(_))
            ));
        }
    }

    #[test]
    fn a_memory_store_keeps_one_secret_per_provider() {
        let mut store = MemoryStore::default();
        store
            .set("example", &Secret::new("first-secret".to_owned()).unwrap())
            .unwrap();
        store
            .set("example", &Secret::new("second-secret".to_owned()).unwrap())
            .unwrap();
        assert_eq!(
            store.get("example").unwrap().unwrap().expose(),
            "second-secret"
        );
        assert!(!format!("{store:?}").contains("second-secret"));
        assert!(store.delete("example").unwrap());
        assert!(!store.delete("example").unwrap());
        assert!(store.get("example").unwrap().is_none());
    }
}
