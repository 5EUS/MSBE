//! Signing keys and signed envelopes for publishing WebAssembly pack codecs.
//!
//! A publisher generates a key once, signs each module it releases, and gives users the trust entry
//! for the key's public half. Signing writes exactly the envelope document MSBE reads from
//! `<home>/extensions/codecs/`, and verifying checks a signed codec the way installing it would
//! (`docs/18-wasm-extensions.md` §18.3).

use std::{
    ffi::OsStr,
    fmt, fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use msbe_core::config::Home;
use msbe_provider_api::{
    EnvelopeError, ExtensionEnvelope, ExtensionProvide, HostApiRange, PackCodec as _,
    PackCodecError, SigningKey, hex,
};
use msbe_wasm_codec::WasmPackCodec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Providers, RegistryError, installed};

/// The host API a signed codec declares.
const HOST_API: HostApiRange = HostApiRange {
    minimum: 1,
    maximum: 1,
};
/// The most bytes a key file may be.
const KEY_LIMIT: u64 = 4096;
/// The longest signer ID.
const SIGNER_LIMIT: usize = 64;
/// The comment every key file starts with.
const KEY_HEADER: &str = "# An MSBE extension signing key. Anyone who holds this file can sign as its signer:\n# keep it private, back it up, and never commit it.\n";

/// An Ed25519 key that signs extensions as one signer.
pub struct SignerKey {
    signer: String,
    key: SigningKey,
}

impl fmt::Debug for SignerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignerKey")
            .field("signer", &self.signer)
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyDocument {
    signer: String,
    secret: String,
}

impl SignerKey {
    /// A new key for `signer`, from the operating system's secure random source.
    ///
    /// # Errors
    ///
    /// Returns [`AuthoringError::InvalidSigner`] for an unusable signer ID, and
    /// [`AuthoringError::Randomness`] when no secure randomness is available.
    pub fn generate(signer: &str) -> Result<Self, AuthoringError> {
        validate_signer(signer)?;
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret)
            .map_err(|error| AuthoringError::Randomness(error.to_string()))?;
        let key = SigningKey::from_bytes(&secret);
        secret.fill(0);
        Ok(Self {
            signer: signer.to_owned(),
            key,
        })
    }

    /// Reads the key file at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthoringError::ExposedKey`] on Unix when other users can read the file, and
    /// [`AuthoringError::InvalidKey`] when it is not a signing key.
    pub fn read(path: &Path) -> Result<Self, AuthoringError> {
        let metadata = fs::metadata(path).map_err(|source| io_error("read", path, source))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(AuthoringError::ExposedKey(path.to_path_buf()));
            }
        }
        if metadata.len() > KEY_LIMIT {
            return Err(invalid_key(path, "the file is too large"));
        }
        let text = fs::read_to_string(path).map_err(|source| io_error("read", path, source))?;
        let document: KeyDocument =
            toml::from_str(&text).map_err(|error| invalid_key(path, error))?;
        validate_signer(&document.signer)?;
        let secret = installed::decode_bytes(&document.secret)
            .ok_or_else(|| invalid_key(path, "its secret is not 64 hexadecimal characters"))?;
        Ok(Self {
            signer: document.signer,
            key: SigningKey::from_bytes(&secret),
        })
    }

    /// Writes this key to a new file at `path`, readable only by its owner on Unix.
    ///
    /// # Errors
    ///
    /// Returns [`AuthoringError::KeyExists`] when `path` exists: a key file is never replaced.
    pub fn write_new(&self, path: &Path) -> Result<(), AuthoringError> {
        let document = toml::to_string(&KeyDocument {
            signer: self.signer.clone(),
            secret: hex(self.key.as_bytes()),
        })
        .map_err(|error| AuthoringError::Encode(error.to_string()))?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path).map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                AuthoringError::KeyExists(path.to_path_buf())
            } else {
                io_error("create", path, source)
            }
        })?;
        file.write_all(format!("{KEY_HEADER}{document}").as_bytes())
            .map_err(|source| io_error("write", path, source))?;
        file.sync_all()
            .map_err(|source| io_error("fsync", path, source))
    }

    /// The signer this key signs as.
    pub fn signer(&self) -> &str {
        &self.signer
    }

    /// The hexadecimal public key users add to their trust root.
    pub fn public_key(&self) -> String {
        hex(self.key.verifying_key().as_bytes())
    }

    /// The `trust.toml` entry that trusts this key to publish provider-neutral codecs, and codecs
    /// bound to `providers`.
    pub fn trust_entry(&self, providers: &[String]) -> String {
        let quoted = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let providers = if providers.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = providers.iter().map(|provider| quoted(provider)).collect();
            format!("providers = [{}]\n", names.join(", "))
        };
        format!(
            "[[signer]]\nid = {}\nkey = \"{}\"\n{providers}",
            quoted(&self.signer),
            self.public_key()
        )
    }
}

/// A codec [`sign_codec`] signed.
#[derive(Debug, Clone, Serialize)]
pub struct SignedCodec {
    /// The envelope document written beside the module.
    pub envelope: PathBuf,
    /// The module it signs.
    pub module: PathBuf,
    /// The extension ID.
    pub id: String,
    /// The extension version.
    pub version: String,
    /// The codec ID the module declares.
    pub codec: String,
    /// The provider the codec binds to, which its signer must be granted.
    pub provider: Option<String>,
    /// The signer.
    pub signer: String,
    /// The SHA-256 package digest the signature covers.
    pub package_digest: String,
}

/// Signs the pack codec at `module` with `key`, writing its envelope document beside it as
/// `<name>.toml` and replacing any earlier envelope there.
///
/// The module is loaded in the sandbox first, so only a module MSBE can run as a codec is signed.
/// The extension ID defaults to the codec ID the module declares.
///
/// # Errors
///
/// Returns [`AuthoringError::ModuleName`] unless `module` names a `.wasm` file,
/// [`AuthoringError::Codec`] when it is not a usable codec, and another [`AuthoringError`] when
/// the envelope cannot be signed or written.
pub fn sign_codec(
    module: &Path,
    key: &SignerKey,
    version: &str,
    id: Option<&str>,
) -> Result<SignedCodec, AuthoringError> {
    let name = module
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| installed::is_module_name(name))
        .ok_or_else(|| AuthoringError::ModuleName(module.to_path_buf()))?;
    let payload = installed::read_limited(module, installed::MODULE_LIMIT)?
        .ok_or_else(|| io_error("read", module, io::Error::from(io::ErrorKind::NotFound)))?;
    let codec = WasmPackCodec::load(&payload)?;
    let descriptor = codec.descriptor();
    let mut envelope = ExtensionEnvelope {
        schema: 1,
        package_digest: ExtensionEnvelope::package_digest_for(&payload)?,
        id: id.map_or_else(|| descriptor.id.clone(), str::to_owned),
        version: version.to_owned(),
        provides: vec![ExtensionProvide::PackCodecV1],
        host_api: HOST_API,
        capabilities: Vec::new(),
        signer: key.signer.clone(),
        signature: "00".repeat(64),
        payload,
    };
    envelope.sign(&key.key)?;
    let path = module.with_extension("toml");
    let document = installed::envelope_document(&envelope, name)
        .map_err(|error| AuthoringError::Encode(error.to_string()))?;
    msbe_fsops::atomic::write_file(&path, document.as_bytes())?;
    Ok(SignedCodec {
        envelope: path,
        module: module.to_path_buf(),
        id: envelope.id,
        version: envelope.version,
        codec: descriptor.id.clone(),
        provider: descriptor.provider.clone(),
        signer: envelope.signer,
        package_digest: envelope.package_digest,
    })
}

/// A signed codec [`verify_codec`] found acceptable.
#[derive(Debug, Clone, Serialize)]
pub struct VerifiedCodec {
    /// The envelope document.
    pub envelope: PathBuf,
    /// The extension ID.
    pub id: String,
    /// The extension version.
    pub version: String,
    /// The codec ID the module declares.
    pub codec: String,
    /// The provider the codec binds to.
    pub provider: Option<String>,
    /// The signer.
    pub signer: String,
    /// The trust root that accepts it.
    pub trust: PathBuf,
}

/// Checks the signed codec whose envelope document is at `envelope` against `home`'s trust root,
/// as installing it would: its module and signature, its signer's trust and provider grant, and
/// that its codec ID and detection hints are free among the codecs MSBE ships. Nothing is
/// installed.
///
/// # Errors
///
/// Returns [`AuthoringError::Registry`] naming why the codec would be refused.
pub fn verify_codec(home: &Home, envelope: &Path) -> Result<VerifiedCodec, AuthoringError> {
    let trust = installed::read_trust(&home.root().join(installed::DIRECTORY))?;
    let signed = installed::read_codec(envelope.to_path_buf())?;
    let mut providers = Providers::builtins()?;
    let codec = providers.register_wasm_codec(&signed.envelope, &trust)?;
    let provider = providers.pack_codec(&codec)?.descriptor().provider.clone();
    Ok(VerifiedCodec {
        envelope: signed.path,
        id: signed.envelope.id,
        version: signed.envelope.version,
        codec,
        provider,
        signer: signed.envelope.signer,
        trust: installed::trust_file(home),
    })
}

/// Why a key could not be made or used, or a codec could not be signed or verified.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthoringError {
    /// A signer ID is not usable.
    #[error("signer {0:?} must be 1 to 64 ASCII letters, digits, '.', '_' or '-'")]
    InvalidSigner(String),
    /// The operating system could not supply randomness for a key.
    #[error("cannot generate a key without secure randomness: {0}")]
    Randomness(String),
    /// A key file already exists where a new key was to be written.
    #[error("{} already exists; a key file is never replaced", .0.display())]
    KeyExists(PathBuf),
    /// Other users can read a key file.
    #[error("{} can be read by other users; restrict it with `chmod 600` before signing", .0.display())]
    ExposedKey(PathBuf),
    /// A key file is not a signing key.
    #[error("{} is not an MSBE signing key: {reason}", .path.display())]
    InvalidKey {
        /// The key file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// A module path does not name a `.wasm` file.
    #[error("{} is not a .wasm module", .0.display())]
    ModuleName(PathBuf),
    /// A file could not be read or written.
    #[error("cannot {action} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The envelope could not be written.
    #[error(transparent)]
    Fs(#[from] msbe_fsops::Error),
    /// A key or envelope could not be encoded.
    #[error("cannot encode: {0}")]
    Encode(String),
    /// The module is not a codec MSBE can run.
    #[error("the module is not a usable pack codec: {0}")]
    Codec(#[from] PackCodecError),
    /// The envelope could not be signed.
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// The signed codec would be refused.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Refuses a signer ID that is empty, long, or would need quoting to read.
fn validate_signer(signer: &str) -> Result<(), AuthoringError> {
    let usable = !signer.is_empty()
        && signer.len() <= SIGNER_LIMIT
        && signer
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if usable {
        Ok(())
    } else {
        Err(AuthoringError::InvalidSigner(signer.to_owned()))
    }
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> AuthoringError {
    AuthoringError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

fn invalid_key(path: &Path, reason: impl fmt::Display) -> AuthoringError {
    AuthoringError::InvalidKey {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use msbe_core::config::Home;

    use super::{AuthoringError, SignerKey, sign_codec, verify_codec};
    use crate::RegistryError;

    const PACK_LIST: &[u8] = include_bytes!("../../msbe-wasm-codec/tests/fixtures/pack-list.wasm");

    fn trust(home: &Path, key: &SignerKey) {
        std::fs::create_dir_all(home.join("extensions")).unwrap();
        std::fs::write(home.join("extensions/trust.toml"), key.trust_entry(&[])).unwrap();
    }

    #[test]
    fn keys_round_trip_are_private_and_are_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publisher.toml");
        let key = SignerKey::generate("publisher").unwrap();
        key.write_new(&path).unwrap();
        assert!(matches!(
            SignerKey::generate("publisher").unwrap().write_new(&path),
            Err(AuthoringError::KeyExists(_))
        ));
        let read = SignerKey::read(&path).unwrap();
        assert_eq!(read.signer(), "publisher");
        assert_eq!(read.public_key(), key.public_key());
        assert!(!format!("{read:?}").contains(&super::hex(key.key.as_bytes())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                SignerKey::read(&path),
                Err(AuthoringError::ExposedKey(_))
            ));
        }
        for signer in ["", "has space", "ünïcode", &"x".repeat(65)] {
            assert!(matches!(
                SignerKey::generate(signer),
                Err(AuthoringError::InvalidSigner(_))
            ));
        }
    }

    #[test]
    fn signed_codecs_verify_only_under_a_trust_root_that_names_their_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let module = dir.path().join("pack-list.wasm");
        std::fs::write(&module, PACK_LIST).unwrap();
        let key = SignerKey::generate("publisher").unwrap();

        let signed = sign_codec(&module, &key, "1.2.0", None).unwrap();
        assert_eq!(signed.envelope, dir.path().join("pack-list.toml"));
        assert_eq!(
            (signed.id.as_str(), signed.codec.as_str()),
            ("pack-list", "pack-list")
        );
        assert_eq!(signed.provider, None);

        assert!(matches!(
            verify_codec(&home, &signed.envelope),
            Err(AuthoringError::Registry(RegistryError::PackCodec(_)))
        ));
        trust(home.root(), &key);
        let verified = verify_codec(&home, &signed.envelope).unwrap();
        assert_eq!(
            (
                verified.codec.as_str(),
                verified.version.as_str(),
                verified.signer.as_str()
            ),
            ("pack-list", "1.2.0", "publisher")
        );

        let mut tampered = PACK_LIST.to_vec();
        tampered.push(0);
        std::fs::write(&module, tampered).unwrap();
        assert!(verify_codec(&home, &signed.envelope).is_err());
    }

    #[test]
    fn only_wasm_codecs_are_signed() {
        let dir = tempfile::tempdir().unwrap();
        let key = SignerKey::generate("publisher").unwrap();
        let text = dir.path().join("pack-list.txt");
        std::fs::write(&text, PACK_LIST).unwrap();
        assert!(matches!(
            sign_codec(&text, &key, "1.0.0", None),
            Err(AuthoringError::ModuleName(_))
        ));
        let junk = dir.path().join("junk.wasm");
        std::fs::write(&junk, b"not a module").unwrap();
        assert!(matches!(
            sign_codec(&junk, &key, "1.0.0", None),
            Err(AuthoringError::Codec(_))
        ));
        assert!(!dir.path().join("junk.toml").exists());
    }
}
