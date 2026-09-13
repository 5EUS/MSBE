//! WebAssembly pack codecs installed in MSBE's data directory, and the signers local policy trusts
//! to publish them.
//!
//! ```text
//! <home>/extensions/
//!   trust.toml            [[signer]] id, key (hexadecimal Ed25519 public key), providers
//!   codecs/<name>.toml    a signed codec envelope naming its module
//!   codecs/<name>.wasm
//! ```
//!
//! See `docs/18-wasm-extensions.md` §18.3.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use msbe_core::config::Home;
use msbe_provider_api::{
    ExtensionCapability, ExtensionEnvelope, ExtensionProvide, HostApiRange, VerifyingKey,
};
use serde::{Deserialize, Serialize};

use crate::RegistryError;

/// The data-directory folder installed extensions live in.
pub(crate) const DIRECTORY: &str = "extensions";
/// The most bytes a codec module may be.
pub(crate) const MODULE_LIMIT: u64 = 64 << 20;

const TRUST_FILE: &str = "trust.toml";
const CODECS: &str = "codecs";
/// The most bytes a trust root or envelope document may be.
const DOCUMENT_LIMIT: u64 = 1 << 20;

/// The trust root in `home`: the signers this installation accepts extensions from.
pub fn trust_file(home: &Home) -> PathBuf {
    home.root().join(DIRECTORY).join(TRUST_FILE)
}

/// The folder in `home` that installed codecs, envelope and module side by side, are read from.
pub fn codecs_directory(home: &Home) -> PathBuf {
    home.root().join(DIRECTORY).join(CODECS)
}

/// Signers local policy trusts to publish extensions, and the providers each may bind codecs to.
///
/// Every trusted signer may publish provider-neutral codecs. A codec whose descriptor names a
/// provider also needs its signer to be granted that provider, because it is then served under
/// that provider's identity and policy.
#[derive(Debug, Clone, Default)]
pub struct ExtensionTrust {
    signers: BTreeMap<String, TrustedSigner>,
}

#[derive(Debug, Clone)]
struct TrustedSigner {
    key: VerifyingKey,
    providers: BTreeSet<String>,
}

impl ExtensionTrust {
    /// This trust, also trusting `signer`, verified by `key`, to publish codecs bound to
    /// `providers` as well as provider-neutral ones.
    #[must_use]
    pub fn with_signer(
        mut self,
        signer: impl Into<String>,
        key: VerifyingKey,
        providers: impl IntoIterator<Item = String>,
    ) -> Self {
        self.signers.insert(
            signer.into(),
            TrustedSigner {
                key,
                providers: providers.into_iter().collect(),
            },
        );
        self
    }

    /// The verifying key of every trusted signer.
    pub fn keys(&self) -> BTreeMap<String, VerifyingKey> {
        self.signers
            .iter()
            .map(|(id, signer)| (id.clone(), signer.key))
            .collect()
    }

    /// Whether `signer` may publish codecs bound to `provider`.
    pub fn may_publish_for(&self, signer: &str, provider: &str) -> bool {
        self.signers
            .get(signer)
            .is_some_and(|trusted| trusted.providers.contains(provider))
    }
}

/// A codec envelope as installed: the envelope fields, and the module file its payload is.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodecDocument {
    schema: u32,
    id: String,
    version: String,
    module: String,
    package_digest: String,
    provides: Vec<ExtensionProvide>,
    #[serde(default)]
    capabilities: Vec<ExtensionCapability>,
    signer: String,
    signature: String,
    host_api: HostApiRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustDocument {
    #[serde(default)]
    signer: Vec<SignerDocument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignerDocument {
    id: String,
    key: String,
    #[serde(default)]
    providers: Vec<String>,
}

/// An installed codec's envelope, with its module bytes as the payload.
pub(crate) struct InstalledCodec {
    /// The envelope document it was read from.
    pub(crate) path: PathBuf,
    pub(crate) envelope: ExtensionEnvelope<Vec<u8>>,
}

/// Everything installed beneath an extensions directory.
pub(crate) struct Installed {
    pub(crate) trust: ExtensionTrust,
    /// Codecs in envelope file name order.
    pub(crate) codecs: Vec<InstalledCodec>,
}

/// Reads the trust root and every codec envelope beneath `directory`. A missing directory, trust
/// root or codecs folder is simply empty.
pub(crate) fn read(directory: &Path) -> Result<Installed, RegistryError> {
    let trust = read_trust(directory)?;
    let codecs_directory = directory.join(CODECS);
    let entries = match fs::read_dir(&codecs_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Installed {
                trust,
                codecs: Vec::new(),
            });
        }
        Err(error) => return Err(invalid(&codecs_directory, error)),
    };
    let mut documents = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| invalid(&codecs_directory, error))?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            documents.push(path);
        }
    }
    documents.sort();
    let codecs = documents
        .into_iter()
        .map(read_codec)
        .collect::<Result<_, _>>()?;
    Ok(Installed { trust, codecs })
}

/// The trust root beneath `directory`, or no trust at all when it has none.
pub(crate) fn read_trust(directory: &Path) -> Result<ExtensionTrust, RegistryError> {
    let path = directory.join(TRUST_FILE);
    match read_limited(&path, DOCUMENT_LIMIT)? {
        Some(bytes) => parse_trust(&path, &bytes),
        None => Ok(ExtensionTrust::default()),
    }
}

/// The codec whose envelope document is at `path`, with the module it names beside it.
pub(crate) fn read_codec(path: PathBuf) -> Result<InstalledCodec, RegistryError> {
    let bytes = read_limited(&path, DOCUMENT_LIMIT)?
        .ok_or_else(|| invalid(&path, "the envelope does not exist"))?;
    let text = std::str::from_utf8(&bytes).map_err(|error| invalid(&path, error))?;
    let document: CodecDocument = toml::from_str(text).map_err(|error| invalid(&path, error))?;
    if !is_module_name(&document.module) {
        return Err(invalid(
            &path,
            format!(
                "module {:?} must be the name of a .wasm file beside the envelope",
                document.module
            ),
        ));
    }
    let module = path.with_file_name(&document.module);
    let payload = read_limited(&module, MODULE_LIMIT)?
        .ok_or_else(|| invalid(&path, format!("module {} is missing", document.module)))?;
    Ok(InstalledCodec {
        envelope: ExtensionEnvelope {
            schema: document.schema,
            package_digest: document.package_digest,
            id: document.id,
            version: document.version,
            provides: document.provides,
            host_api: document.host_api,
            capabilities: document.capabilities,
            signer: document.signer,
            signature: document.signature,
            payload,
        },
        path,
    })
}

/// The envelope document for `envelope`, whose payload is the module named `module` beside it.
pub(crate) fn envelope_document(
    envelope: &ExtensionEnvelope<Vec<u8>>,
    module: &str,
) -> Result<String, toml::ser::Error> {
    toml::to_string(&CodecDocument {
        schema: envelope.schema,
        id: envelope.id.clone(),
        version: envelope.version.clone(),
        module: module.to_owned(),
        package_digest: envelope.package_digest.clone(),
        provides: envelope.provides.clone(),
        capabilities: envelope.capabilities.clone(),
        signer: envelope.signer.clone(),
        signature: envelope.signature.clone(),
        host_api: envelope.host_api,
    })
}

fn parse_trust(path: &Path, bytes: &[u8]) -> Result<ExtensionTrust, RegistryError> {
    let text = std::str::from_utf8(bytes).map_err(|error| invalid(path, error))?;
    let document: TrustDocument = toml::from_str(text).map_err(|error| invalid(path, error))?;
    let mut trust = ExtensionTrust::default();
    for signer in document.signer {
        if trust.signers.contains_key(&signer.id) {
            return Err(invalid(
                path,
                format!("signer {:?} is listed more than once", signer.id),
            ));
        }
        let key = decode_bytes(&signer.key)
            .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
            .ok_or_else(|| {
                invalid(
                    path,
                    format!(
                        "signer {:?} needs a 64-character hexadecimal Ed25519 public key",
                        signer.id
                    ),
                )
            })?;
        trust = trust.with_signer(signer.id, key, signer.providers);
    }
    Ok(trust)
}

/// The file at `path`, or `None` when it does not exist, refusing one larger than `limit`.
pub(crate) fn read_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, RegistryError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(invalid(path, error)),
    };
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(path, error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(invalid(path, format!("larger than the {limit}-byte limit")));
    }
    Ok(Some(bytes))
}

/// Whether `name` is a plain `.wasm` file name, with no directory part.
pub(crate) fn is_module_name(name: &str) -> bool {
    !name.starts_with('.')
        && !name.contains(['/', '\\', ':', '\0'])
        && Path::new(name)
            .extension()
            .is_some_and(|extension| extension == "wasm")
}

/// The 32 bytes a 64-character hexadecimal string spells.
pub(crate) fn decode_bytes(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

fn invalid(path: &Path, reason: impl fmt::Display) -> RegistryError {
    RegistryError::InstalledExtension {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}
