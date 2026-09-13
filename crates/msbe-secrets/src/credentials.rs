//! Finding, keeping and forgetting provider credentials across the stores.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use msbe_core::config::Home;
use serde::{Deserialize, Serialize};

use crate::{
    Backend, Clock, EncryptedFileStore, EnvironmentStore, KdfParams, KeyringStore, Secret,
    SecretStore, StoreError, SystemClock, files, store::validate_provider,
};

const INDEX: &str = "credentials.toml";
const INDEX_KIND: &str = "credentials file";
const FILE_STORE: &str = "secrets.toml";
/// How stale a recorded last use may become before it is written again, in seconds.
const USE_GRANULARITY: u64 = 60;

/// Where a provider's credential is kept, and when it was stored and last used. It holds no
/// secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRecord {
    /// The provider id.
    pub provider: String,
    /// The store holding the credential.
    pub backend: Backend,
    /// The account name the provider reported for the credential, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// When it was stored, as Unix seconds.
    pub stored: u64,
    /// When it was last read, as Unix seconds, to within a minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    #[serde(default, rename = "credential")]
    records: Vec<CredentialRecord>,
}

#[derive(Serialize)]
struct IndexRef<'a> {
    #[serde(rename = "credential")]
    records: &'a [CredentialRecord],
}

/// Reads the credential records in `directory`. A missing file has none.
pub(crate) fn read_records(directory: &Path) -> Result<Vec<CredentialRecord>, StoreError> {
    let path = directory.join(INDEX);
    Ok(match files::read_text(&path, INDEX_KIND)? {
        Some(text) => {
            toml::from_str::<Index>(&text)
                .map_err(|error| files::malformed(&path, INDEX_KIND, error.to_string()))?
                .records
        }
        None => Vec::new(),
    })
}

/// The keyring, opened the first time it is needed.
#[derive(Debug)]
enum Keyring {
    Unopened,
    Open(Box<dyn SecretStore>),
    Unavailable(String),
}

/// A data directory's provider credentials.
///
/// A provider's credential is its `MSBE_<PROVIDER>_TOKEN` environment variable when that is set,
/// and otherwise whatever the store `credentials.toml` records for it holds: the keyring, or the
/// encrypted file once it is unlocked. A new credential goes to the keyring when one can be
/// reached, and otherwise to the unlocked encrypted file.
pub struct Credentials {
    directory: PathBuf,
    records: Vec<CredentialRecord>,
    environment: EnvironmentStore,
    keyring: Keyring,
    file: Option<Box<dyn SecretStore>>,
    clock: Box<dyn Clock>,
}

impl Credentials {
    /// `home`'s credentials, with this process's environment, the platform keyring, and the system
    /// clock. The keyring is not contacted until a credential needs it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when `credentials.toml` cannot be read.
    pub fn open(home: &Home) -> Result<Self, StoreError> {
        let directory = files::auth_directory(home);
        Ok(Self {
            records: read_records(&directory)?,
            directory,
            environment: EnvironmentStore::process(),
            keyring: Keyring::Unopened,
            file: None,
            clock: Box::new(SystemClock),
        })
    }

    /// Credentials recorded in `directory`, with tokens from `environment` and `keyring` in place of
    /// the platform keyring, or no keyring when it is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when `credentials.toml` cannot be read.
    pub fn with_stores(
        directory: &Path,
        environment: EnvironmentStore,
        keyring: Option<Box<dyn SecretStore>>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            records: read_records(directory)?,
            directory: directory.to_path_buf(),
            environment,
            keyring: keyring.map_or_else(
                || Keyring::Unavailable("no keyring is configured".to_owned()),
                Keyring::Open,
            ),
            file: None,
            clock,
        })
    }

    /// Unlocks the encrypted file with `passphrase` for as long as this value lives.
    ///
    /// # Errors
    ///
    /// As for [`EncryptedFileStore::open`].
    pub fn unlock(&mut self, passphrase: &Secret) -> Result<(), StoreError> {
        self.unlock_with(passphrase, KdfParams::RECOMMENDED)
    }

    /// As [`Credentials::unlock`], creating a missing file with `params`.
    ///
    /// # Errors
    ///
    /// As for [`EncryptedFileStore::open_with`].
    pub fn unlock_with(
        &mut self,
        passphrase: &Secret,
        params: KdfParams,
    ) -> Result<(), StoreError> {
        let store =
            EncryptedFileStore::open_with(self.directory.join(FILE_STORE), passphrase, params)?;
        self.file = Some(Box::new(store));
        Ok(())
    }

    /// What `credentials.toml` records, ordered by provider.
    pub fn records(&self) -> &[CredentialRecord] {
        &self.records
    }

    /// Whether `provider`'s token is set in the environment, which overrides a stored credential.
    pub fn in_environment(&self, provider: &str) -> bool {
        self.environment.contains(provider)
    }

    /// `provider`'s credential, if it has one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid token in the environment, or when the store recorded
    /// for the provider cannot be read, including [`StoreError::Locked`] for a locked file.
    pub fn get(&mut self, provider: &str) -> Result<Option<Secret>, StoreError> {
        if let Some(secret) = self.environment.get(provider)? {
            return Ok(Some(secret));
        }
        let Some(backend) = self.record(provider).map(|record| record.backend) else {
            return Ok(None);
        };
        let secret = self.backend_store(backend)?.get(provider)?;
        let now = self.clock.now();
        if secret.is_none() {
            // The store no longer has what was recorded, so the record is forgotten too.
            self.records.retain(|record| record.provider != provider);
            self.save()?;
        } else if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.provider == provider)
            && record
                .last_used
                .is_none_or(|used| now.saturating_sub(used) >= USE_GRANULARITY)
        {
            record.last_used = Some(now);
            self.save()?;
        }
        Ok(secret)
    }

    /// Keeps `secret` as `provider`'s credential, with the account name the provider reported for
    /// it, and returns the store it went to.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NoWritableStore`] when the keyring cannot be reached and the encrypted
    /// file is locked, and otherwise the store's failure.
    pub fn store(
        &mut self,
        provider: &str,
        secret: &Secret,
        account: Option<String>,
    ) -> Result<Backend, StoreError> {
        validate_provider(provider)?;
        let backend = self.keep(provider, secret)?;
        if let Some(previous) = self.record(provider).map(|record| record.backend)
            && previous != backend
        {
            // The credential moved, so the old copy goes. A store that can no longer be reached
            // keeps nothing anyone can use.
            match self
                .backend_store(previous)
                .and_then(|store| store.delete(provider))
            {
                Ok(_) | Err(StoreError::Unavailable { .. } | StoreError::Locked) => {}
                Err(error) => return Err(error),
            }
        }
        let stored = self.clock.now();
        self.records.retain(|record| record.provider != provider);
        self.records.push(CredentialRecord {
            provider: provider.to_owned(),
            backend,
            account,
            stored,
            last_used: None,
        });
        self.records
            .sort_by(|left, right| left.provider.cmp(&right.provider));
        self.save()?;
        Ok(backend)
    }

    /// Forgets `provider`'s stored credential, reporting whether it had one. A token in the
    /// environment is unaffected.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store holding the credential cannot be written.
    pub fn remove(&mut self, provider: &str) -> Result<bool, StoreError> {
        validate_provider(provider)?;
        let Some(backend) = self.record(provider).map(|record| record.backend) else {
            return Ok(false);
        };
        self.backend_store(backend)?.delete(provider)?;
        self.records.retain(|record| record.provider != provider);
        self.save()?;
        Ok(true)
    }

    fn record(&self, provider: &str) -> Option<&CredentialRecord> {
        self.records
            .iter()
            .find(|record| record.provider == provider)
    }

    /// Keeps `secret` in the keyring, or in the unlocked file when the keyring cannot be reached.
    fn keep(&mut self, provider: &str, secret: &Secret) -> Result<Backend, StoreError> {
        let kept = match self.keyring() {
            Ok(keyring) => {
                let backend = keyring.backend();
                keyring.set(provider, secret).map(|()| backend)
            }
            Err(error) => Err(error),
        };
        match kept {
            Err(StoreError::Unavailable { .. }) => match self.file.as_deref_mut() {
                Some(file) => {
                    let backend = file.backend();
                    file.set(provider, secret).map(|()| backend)
                }
                None => Err(StoreError::NoWritableStore),
            },
            kept => kept,
        }
    }

    /// The open store that keeps `backend`'s credentials.
    fn backend_store(
        &mut self,
        backend: Backend,
    ) -> Result<&mut (dyn SecretStore + 'static), StoreError> {
        if self
            .file
            .as_ref()
            .is_some_and(|file| file.backend() == backend)
        {
            return self.file.as_deref_mut().ok_or(StoreError::Locked);
        }
        if backend == Backend::EncryptedFile {
            return Err(StoreError::Locked);
        }
        let keyring = self.keyring()?;
        if keyring.backend() == backend {
            Ok(keyring)
        } else {
            Err(StoreError::Unavailable {
                backend,
                reason: "no such store is open".to_owned(),
            })
        }
    }

    /// The keyring, opening the platform keyring the first time.
    fn keyring(&mut self) -> Result<&mut (dyn SecretStore + 'static), StoreError> {
        if matches!(self.keyring, Keyring::Unopened) {
            self.keyring = match KeyringStore::platform() {
                Ok(store) => Keyring::Open(Box::new(store)),
                Err(StoreError::Unavailable { reason, .. }) => Keyring::Unavailable(reason),
                Err(error) => Keyring::Unavailable(error.to_string()),
            };
        }
        match &mut self.keyring {
            Keyring::Open(store) => Ok(store.as_mut()),
            Keyring::Unavailable(reason) => Err(StoreError::Unavailable {
                backend: Backend::Keyring,
                reason: reason.clone(),
            }),
            Keyring::Unopened => Err(StoreError::Unavailable {
                backend: Backend::Keyring,
                reason: "the keyring was not opened".to_owned(),
            }),
        }
    }

    fn save(&self) -> Result<(), StoreError> {
        let path = self.directory.join(INDEX);
        let text = toml::to_string(&IndexRef {
            records: &self.records,
        })
        .map_err(|error| files::malformed(&path, INDEX_KIND, error.to_string()))?;
        files::write_private(&path, text.as_bytes())
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("directory", &self.directory)
            .field("records", &self.records)
            .field("keyring", &self.keyring)
            .field("file_unlocked", &self.file.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Credentials;
    use crate::{Backend, Clock, EnvironmentStore, KdfParams, MemoryStore, Secret, StoreError};

    const CHEAP: KdfParams = KdfParams {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    };

    #[derive(Debug)]
    struct Fixed(u64);

    impl Clock for Fixed {
        fn now(&self) -> u64 {
            self.0
        }
    }

    fn environment() -> EnvironmentStore {
        EnvironmentStore::with_lookup(|provider| {
            (provider == "from-env").then(|| "environment-token-1".to_owned())
        })
    }

    fn secret(value: &str) -> Secret {
        Secret::new(value.to_owned()).unwrap()
    }

    #[test]
    fn credentials_go_to_the_keyring_and_are_recorded_without_their_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut credentials = Credentials::with_stores(
            dir.path(),
            environment(),
            Some(Box::new(MemoryStore::default())),
            Box::new(Fixed(1_000)),
        )
        .unwrap();
        let backend = credentials
            .store(
                "example",
                &secret("stored-token-1"),
                Some("someone".to_owned()),
            )
            .unwrap();
        assert_eq!(backend, Backend::Memory);
        assert_eq!(
            credentials.get("example").unwrap().unwrap().expose(),
            "stored-token-1"
        );
        let record = credentials.records().first().unwrap();
        assert_eq!(record.account.as_deref(), Some("someone"));
        assert_eq!((record.stored, record.last_used), (1_000, Some(1_000)));
        let index = fs::read_to_string(dir.path().join("credentials.toml")).unwrap();
        assert!(index.contains("example") && !index.contains("stored-token-1"));
        assert!(!format!("{credentials:?}").contains("stored-token-1"));

        assert!(credentials.remove("example").unwrap());
        assert!(!credentials.remove("example").unwrap());
        assert!(credentials.get("example").unwrap().is_none());
    }

    #[test]
    fn the_environment_overrides_a_stored_credential() {
        let dir = tempfile::tempdir().unwrap();
        let mut credentials = Credentials::with_stores(
            dir.path(),
            environment(),
            Some(Box::new(MemoryStore::default())),
            Box::new(Fixed(1)),
        )
        .unwrap();
        credentials
            .store("from-env", &secret("stored-token-2"), None)
            .unwrap();
        assert!(credentials.in_environment("from-env"));
        assert_eq!(
            credentials.get("from-env").unwrap().unwrap().expose(),
            "environment-token-1"
        );
    }

    #[test]
    fn a_record_whose_store_lost_the_credential_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Credentials::with_stores(
            dir.path(),
            environment(),
            Some(Box::new(MemoryStore::default())),
            Box::new(Fixed(1)),
        )
        .unwrap();
        first
            .store("example", &secret("stored-token-3"), None)
            .unwrap();
        let mut second = Credentials::with_stores(
            dir.path(),
            environment(),
            Some(Box::new(MemoryStore::default())),
            Box::new(Fixed(2)),
        )
        .unwrap();
        assert_eq!(second.records().len(), 1);
        assert!(second.get("example").unwrap().is_none());
        assert!(second.records().is_empty());
    }

    #[test]
    fn without_a_keyring_credentials_need_the_unlocked_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut credentials =
            Credentials::with_stores(dir.path(), environment(), None, Box::new(Fixed(5))).unwrap();
        assert!(matches!(
            credentials.store("example", &secret("stored-token-4"), None),
            Err(StoreError::NoWritableStore)
        ));
        credentials
            .unlock_with(&secret("correct horse"), CHEAP)
            .unwrap();
        assert_eq!(
            credentials
                .store("example", &secret("stored-token-4"), None)
                .unwrap(),
            Backend::EncryptedFile
        );

        let mut reopened =
            Credentials::with_stores(dir.path(), environment(), None, Box::new(Fixed(6))).unwrap();
        assert!(matches!(reopened.get("example"), Err(StoreError::Locked)));
        reopened.unlock(&secret("correct horse")).unwrap();
        assert_eq!(
            reopened.get("example").unwrap().unwrap().expose(),
            "stored-token-4"
        );
    }
}
