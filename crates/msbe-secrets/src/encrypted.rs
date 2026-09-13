//! The encrypted file store: the headless fallback when no keyring can be reached.
//!
//! The file is TOML naming its format, the Argon2id costs and salt that derive its key from a
//! passphrase, and an XChaCha20-Poly1305 nonce and ciphertext. Every header field is authenticated
//! with the ciphertext, so an altered header fails exactly like a wrong passphrase. Each write uses
//! a fresh nonce under the key derived when the store was opened.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{AeadInOut as _, KeyInit as _, XChaCha20Poly1305};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{Backend, Secret, SecretStore, StoreError, files, store::validate_provider};

const FORMAT: u32 = 1;
const KDF: &str = "argon2id";
const CIPHER: &str = "xchacha20-poly1305";
const KIND: &str = "encrypted secrets file";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;

/// Argon2id costs for deriving a file's key from its passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory, in kibibytes.
    pub memory_kib: u32,
    /// Passes over the memory.
    pub iterations: u32,
    /// Lanes.
    pub parallelism: u32,
}

impl KdfParams {
    /// 64 mebibytes and three passes: RFC 9106's second recommendation, on one lane.
    pub const RECOMMENDED: Self = Self {
        memory_kib: 64 * 1024,
        iterations: 3,
        parallelism: 1,
    };

    /// The most memory a file may ask for, so an altered file cannot exhaust the machine.
    const MAX_MEMORY_KIB: u32 = 4 * 1024 * 1024;
    const MAX_ITERATIONS: u32 = 64;
    const MAX_PARALLELISM: u32 = 16;

    /// The key derivation these costs describe, when they are in range.
    fn argon2(self) -> Option<Argon2<'static>> {
        if self.memory_kib > Self::MAX_MEMORY_KIB
            || self.iterations > Self::MAX_ITERATIONS
            || self.parallelism > Self::MAX_PARALLELISM
        {
            return None;
        }
        let params = Params::new(
            self.memory_kib,
            self.iterations,
            self.parallelism,
            Some(KEY_LEN),
        )
        .ok()?;
        Some(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

/// The file as written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format: u32,
    kdf: String,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    salt: String,
    cipher: String,
    nonce: String,
    ciphertext: String,
}

/// What the ciphertext holds, as read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Contents {
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

/// What the ciphertext holds, as written, borrowing every value so none is copied.
#[derive(Serialize)]
struct ContentsRef<'a> {
    secrets: BTreeMap<&'a str, &'a str>,
}

/// Secrets encrypted in a file, under a key derived from a passphrase.
pub struct EncryptedFileStore {
    path: PathBuf,
    params: KdfParams,
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
    secrets: BTreeMap<String, Zeroizing<String>>,
}

impl EncryptedFileStore {
    /// Opens the file at `path` with `passphrase`. A missing file is an empty store, written with
    /// [`KdfParams::RECOMMENDED`] when it first changes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WrongPassphrase`] when the passphrase does not open the file or the
    /// file was altered, and [`StoreError::Malformed`] or [`StoreError::Io`] when it cannot be read.
    pub fn open(path: impl Into<PathBuf>, passphrase: &Secret) -> Result<Self, StoreError> {
        Self::open_with(path, passphrase, KdfParams::RECOMMENDED)
    }

    /// As [`EncryptedFileStore::open`], creating a missing file with `params`. An existing file
    /// keeps the costs it was written with.
    ///
    /// # Errors
    ///
    /// As for [`EncryptedFileStore::open`], and [`StoreError::Malformed`] for costs out of range.
    pub fn open_with(
        path: impl Into<PathBuf>,
        passphrase: &Secret,
        params: KdfParams,
    ) -> Result<Self, StoreError> {
        let path = path.into();
        if let Some(text) = files::read_text(&path, KIND)? {
            return Self::decrypt(path, &text, passphrase);
        }
        let mut salt = [0; SALT_LEN];
        random(&mut salt)?;
        let key = derive(&path, params, &salt, passphrase)?;
        Ok(Self {
            path,
            params,
            salt,
            key,
            secrets: BTreeMap::new(),
        })
    }

    fn decrypt(path: PathBuf, text: &str, passphrase: &Secret) -> Result<Self, StoreError> {
        let document: Document = toml::from_str(text)
            .map_err(|error| files::malformed(&path, KIND, error.to_string()))?;
        if document.format != FORMAT || document.kdf != KDF || document.cipher != CIPHER {
            return Err(files::malformed(
                &path,
                KIND,
                format!(
                    "format {} with {} and {} is not supported",
                    document.format, document.kdf, document.cipher
                ),
            ));
        }
        let params = KdfParams {
            memory_kib: document.memory_kib,
            iterations: document.iterations,
            parallelism: document.parallelism,
        };
        let salt: [u8; SALT_LEN] = decode_array(&document.salt)
            .ok_or_else(|| files::malformed(&path, KIND, "its salt is not 16 hexadecimal bytes"))?;
        let nonce: [u8; NONCE_LEN] = decode_array(&document.nonce).ok_or_else(|| {
            files::malformed(&path, KIND, "its nonce is not 24 hexadecimal bytes")
        })?;
        let mut sealed = decode(&document.ciphertext)
            .ok_or_else(|| files::malformed(&path, KIND, "its ciphertext is not hexadecimal"))?;
        let key = derive(&path, params, &salt, passphrase)?;
        let cipher = XChaCha20Poly1305::new((&*key).into());
        if cipher
            .decrypt_in_place((&nonce).into(), &header(params, &salt, &nonce), &mut sealed)
            .is_err()
        {
            return Err(StoreError::WrongPassphrase(path));
        }
        let opened = Zeroizing::new(sealed);
        // Never report the parse error: it would quote the decrypted text.
        let contents: Contents = std::str::from_utf8(&opened)
            .ok()
            .and_then(|plaintext| toml::from_str(plaintext).ok())
            .ok_or_else(|| {
                files::malformed(
                    &path,
                    KIND,
                    "its decrypted contents are not a secrets table",
                )
            })?;
        let mut secrets = BTreeMap::new();
        for (provider, value) in contents.secrets {
            let value = Zeroizing::new(value);
            validate_provider(&provider)?;
            secrets.insert(provider, value);
        }
        Ok(Self {
            path,
            params,
            salt,
            key,
            secrets,
        })
    }

    fn save(&self) -> Result<(), StoreError> {
        let contents = ContentsRef {
            secrets: self
                .secrets
                .iter()
                .map(|(provider, value)| (provider.as_str(), value.as_str()))
                .collect(),
        };
        let plaintext = Zeroizing::new(
            toml::to_string(&contents).map_err(|_| failed("cannot encode the secrets table"))?,
        );
        let mut nonce = [0; NONCE_LEN];
        random(&mut nonce)?;
        // Reserved up front, so appending the tag never reallocates and strands a plaintext copy.
        let mut sealed = Zeroizing::new(Vec::with_capacity(plaintext.len() + TAG_LEN));
        sealed.extend_from_slice(plaintext.as_bytes());
        XChaCha20Poly1305::new((&*self.key).into())
            .encrypt_in_place(
                (&nonce).into(),
                &header(self.params, &self.salt, &nonce),
                &mut *sealed,
            )
            .map_err(|_| failed("cannot encrypt the secrets table"))?;
        let document = Document {
            format: FORMAT,
            kdf: KDF.to_owned(),
            memory_kib: self.params.memory_kib,
            iterations: self.params.iterations,
            parallelism: self.params.parallelism,
            salt: encode(&self.salt),
            cipher: CIPHER.to_owned(),
            nonce: encode(&nonce),
            ciphertext: encode(&sealed),
        };
        let text = toml::to_string(&document).map_err(|_| failed("cannot encode the file"))?;
        files::write_private(&self.path, text.as_bytes())
    }
}

impl fmt::Debug for EncryptedFileStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncryptedFileStore")
            .field("path", &self.path)
            .field("providers", &self.secrets.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl SecretStore for EncryptedFileStore {
    fn backend(&self) -> Backend {
        Backend::EncryptedFile
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
        let previous = self.secrets.insert(
            provider.to_owned(),
            Zeroizing::new(secret.expose().to_owned()),
        );
        if let Err(error) = self.save() {
            match previous {
                Some(previous) => self.secrets.insert(provider.to_owned(), previous),
                None => self.secrets.remove(provider),
            };
            return Err(error);
        }
        Ok(())
    }

    fn delete(&mut self, provider: &str) -> Result<bool, StoreError> {
        validate_provider(provider)?;
        let Some(previous) = self.secrets.remove(provider) else {
            return Ok(false);
        };
        if let Err(error) = self.save() {
            self.secrets.insert(provider.to_owned(), previous);
            return Err(error);
        }
        Ok(true)
    }
}

/// The bytes authenticated with the ciphertext: every header field, so none can be changed.
fn header(params: KdfParams, salt: &[u8], nonce: &[u8]) -> Vec<u8> {
    format!(
        "msbe-secrets format={FORMAT} kdf={KDF} memory_kib={} iterations={} parallelism={} \
         salt={} cipher={CIPHER} nonce={}",
        params.memory_kib,
        params.iterations,
        params.parallelism,
        encode(salt),
        encode(nonce)
    )
    .into_bytes()
}

fn derive(
    path: &Path,
    params: KdfParams,
    salt: &[u8],
    passphrase: &Secret,
) -> Result<Zeroizing<[u8; KEY_LEN]>, StoreError> {
    let argon2 = params
        .argon2()
        .ok_or_else(|| files::malformed(path, KIND, "its key derivation costs are out of range"))?;
    let mut key = Zeroizing::new([0; KEY_LEN]);
    argon2
        .hash_password_into(passphrase.expose().as_bytes(), salt, &mut *key)
        .map_err(|error| failed(&format!("cannot derive the key: {error}")))?;
    Ok(key)
}

fn random(bytes: &mut [u8]) -> Result<(), StoreError> {
    getrandom::fill(bytes).map_err(|error| failed(&format!("no randomness is available: {error}")))
}

fn failed(reason: &str) -> StoreError {
    StoreError::Failed {
        backend: Backend::EncryptedFile,
        reason: reason.to_owned(),
    }
}

fn encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

fn decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| match pair {
            [high, low] => Some(nibble(*high)? << 4 | nibble(*low)?),
            _ => None,
        })
        .collect()
}

fn decode_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    decode(text)?.try_into().ok()
}

fn nibble(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{EncryptedFileStore, KdfParams};
    use crate::{Secret, SecretStore, StoreError};

    /// Costs low enough for tests; never use them for a real file.
    const CHEAP: KdfParams = KdfParams {
        memory_kib: 64,
        iterations: 1,
        parallelism: 1,
    };

    fn passphrase(value: &str) -> Secret {
        Secret::new(value.to_owned()).unwrap()
    }

    #[test]
    fn secrets_survive_reopening_and_never_appear_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth").join("secrets.toml");
        let mut store =
            EncryptedFileStore::open_with(&path, &passphrase("correct horse"), CHEAP).unwrap();
        store
            .set(
                "example",
                &Secret::new("file-secret-alpha".to_owned()).unwrap(),
            )
            .unwrap();
        store
            .set(
                "other",
                &Secret::new("file-secret-beta".to_owned()).unwrap(),
            )
            .unwrap();
        assert!(!format!("{store:?}").contains("file-secret"));

        let written = fs::read_to_string(&path).unwrap();
        assert!(!written.contains("file-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        let mut reopened = EncryptedFileStore::open(&path, &passphrase("correct horse")).unwrap();
        assert_eq!(
            reopened.get("example").unwrap().unwrap().expose(),
            "file-secret-alpha"
        );
        assert!(reopened.delete("example").unwrap());
        let reopened = EncryptedFileStore::open(&path, &passphrase("correct horse")).unwrap();
        assert!(reopened.get("example").unwrap().is_none());
        assert_eq!(
            reopened.get("other").unwrap().unwrap().expose(),
            "file-secret-beta"
        );
    }

    #[test]
    fn a_wrong_passphrase_or_an_altered_file_opens_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        let mut store =
            EncryptedFileStore::open_with(&path, &passphrase("correct horse"), CHEAP).unwrap();
        store
            .set(
                "example",
                &Secret::new("file-secret-gamma".to_owned()).unwrap(),
            )
            .unwrap();
        let original = fs::read_to_string(&path).unwrap();

        assert!(matches!(
            EncryptedFileStore::open(&path, &passphrase("battery staple")),
            Err(StoreError::WrongPassphrase(_))
        ));

        let weakened = original.replace("iterations = 1", "iterations = 2");
        assert_ne!(weakened, original);
        fs::write(&path, weakened).unwrap();
        assert!(matches!(
            EncryptedFileStore::open(&path, &passphrase("correct horse")),
            Err(StoreError::WrongPassphrase(_))
        ));

        let flipped = original.replacen("ciphertext = \"", "ciphertext = \"00", 1);
        fs::write(&path, flipped).unwrap();
        assert!(matches!(
            EncryptedFileStore::open(&path, &passphrase("correct horse")),
            Err(StoreError::WrongPassphrase(_))
        ));
    }

    #[test]
    fn unsupported_or_exhausting_files_are_refused_before_deriving_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        let mut store =
            EncryptedFileStore::open_with(&path, &passphrase("correct horse"), CHEAP).unwrap();
        store
            .set(
                "example",
                &Secret::new("file-secret-delta".to_owned()).unwrap(),
            )
            .unwrap();
        let original = fs::read_to_string(&path).unwrap();

        fs::write(&path, original.replace("format = 1", "format = 2")).unwrap();
        assert!(matches!(
            EncryptedFileStore::open(&path, &passphrase("correct horse")),
            Err(StoreError::Malformed { .. })
        ));

        fs::write(
            &path,
            original.replace("memory_kib = 64", "memory_kib = 4294967295"),
        )
        .unwrap();
        assert!(matches!(
            EncryptedFileStore::open(&path, &passphrase("correct horse")),
            Err(StoreError::Malformed { .. })
        ));
    }
}
