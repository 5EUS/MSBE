//! The platform keyring.

use std::{fmt, sync::Arc};

use keyring_core::{CredentialStore, Entry, Error};

use crate::{Backend, Secret, SecretStore, StoreError, store::validate_provider};

/// The service MSBE's keyring entries are filed under. Each entry's user is a provider id.
pub const KEYRING_SERVICE: &str = "msbe";

/// Secrets kept in a keyring: Secret Service on Linux and the BSDs, the Keychain on macOS, and
/// Credential Manager on Windows.
pub struct KeyringStore {
    store: Arc<CredentialStore>,
}

impl KeyringStore {
    /// The platform keyring.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Unavailable`] when this platform has no supported keyring or it cannot
    /// be reached, such as without a D-Bus session.
    pub fn platform() -> Result<Self, StoreError> {
        platform_store().map(Self::with_store)
    }

    /// Secrets kept in `store`, such as keyring-core's mock store in tests.
    pub fn with_store(store: Arc<CredentialStore>) -> Self {
        Self { store }
    }

    fn entry(&self, provider: &str) -> Result<Entry, StoreError> {
        validate_provider(provider)?;
        self.store
            .build(KEYRING_SERVICE, provider, None)
            .map_err(|error| failure(&error))
    }
}

impl fmt::Debug for KeyringStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyringStore")
            .field("vendor", &self.store.vendor())
            .finish()
    }
}

impl SecretStore for KeyringStore {
    fn backend(&self) -> Backend {
        Backend::Keyring
    }

    fn get(&self, provider: &str) -> Result<Option<Secret>, StoreError> {
        match self.entry(provider)?.get_password() {
            Ok(value) => Ok(Some(Secret::new(value)?)),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(failure(&error)),
        }
    }

    fn set(&mut self, provider: &str, secret: &Secret) -> Result<(), StoreError> {
        self.entry(provider)?
            .set_password(secret.expose())
            .map_err(|error| failure(&error))
    }

    fn delete(&mut self, provider: &str) -> Result<bool, StoreError> {
        match self.entry(provider)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(Error::NoEntry) => Ok(false),
            Err(error) => Err(failure(&error)),
        }
    }
}

/// Describes a keyring failure without the stored bytes some of its variants carry.
fn failure(error: &Error) -> StoreError {
    let reason = match error {
        Error::NoStorageAccess(source) => {
            return StoreError::Unavailable {
                backend: Backend::Keyring,
                reason: source.to_string(),
            };
        }
        Error::PlatformFailure(source) => source.to_string(),
        Error::BadEncoding(_) => "the stored credential is not UTF-8 text".to_owned(),
        Error::BadDataFormat(..) => {
            "the stored credential is not in the expected format".to_owned()
        }
        other => other.to_string(),
    };
    StoreError::Failed {
        backend: Backend::Keyring,
        reason,
    }
}

fn unavailable(error: &Error) -> StoreError {
    StoreError::Unavailable {
        backend: Backend::Keyring,
        reason: error.to_string(),
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn platform_store() -> Result<Arc<CredentialStore>, StoreError> {
    let store: Arc<CredentialStore> =
        zbus_secret_service_keyring_store::Store::new().map_err(|error| unavailable(&error))?;
    Ok(store)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn platform_store() -> Result<Arc<CredentialStore>, StoreError> {
    let store: Arc<CredentialStore> =
        apple_native_keyring_store::keychain::Store::new().map_err(|error| unavailable(&error))?;
    Ok(store)
}

#[cfg(target_os = "windows")]
fn platform_store() -> Result<Arc<CredentialStore>, StoreError> {
    let store: Arc<CredentialStore> =
        windows_native_keyring_store::Store::new().map_err(|error| unavailable(&error))?;
    Ok(store)
}

#[cfg(any(target_os = "android", not(any(unix, target_os = "windows"))))]
fn platform_store() -> Result<Arc<CredentialStore>, StoreError> {
    Err(StoreError::Unavailable {
        backend: Backend::Keyring,
        reason: "this platform has no supported keyring".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use keyring_core::{Error, mock};

    use super::{KeyringStore, failure};
    use crate::{Secret, SecretStore, StoreError};

    #[test]
    fn secrets_round_trip_through_a_keyring() {
        let mut store = KeyringStore::with_store(mock::Store::new().unwrap());
        assert!(store.get("example").unwrap().is_none());
        store
            .set(
                "example",
                &Secret::new("keyring-secret-1".to_owned()).unwrap(),
            )
            .unwrap();
        assert_eq!(
            store.get("example").unwrap().unwrap().expose(),
            "keyring-secret-1"
        );
        assert!(store.delete("example").unwrap());
        assert!(!store.delete("example").unwrap());
        assert!(matches!(
            store.get("Not A Provider"),
            Err(StoreError::InvalidProvider(_))
        ));
    }

    #[test]
    fn failures_never_carry_the_stored_bytes() {
        let described = failure(&Error::BadEncoding(b"stored-secret-bytes".to_vec())).to_string();
        assert!(!described.contains("stored-secret-bytes"));
        assert!(!described.contains("115, 116"));
    }
}
