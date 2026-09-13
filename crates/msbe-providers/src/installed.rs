//! Extensions installed in MSBE's data directory, and the signers local policy trusts to publish
//! them.
//!
//! ```text
//! <home>/extensions/
//!   trust.toml             [[signer]] id, key, providers, programs; [revoked] signers, digests
//!   codecs/<name>.toml     a signed codec envelope naming its module
//!   codecs/<name>.wasm
//!   providers/<id>.toml    a signed provider program envelope, the program its payload
//! ```
//!
//! Each extension is admitted or refused on its own: one that cannot be read, verified or trusted is
//! skipped with the reason, and never stops another or a provider MSBE ships. See
//! `docs/18-wasm-extensions.md` §18.3 and `docs/06-providers-and-policy.md` §6.4.

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

use crate::{ProgramTrust, RegistryError};

/// The data-directory folder installed extensions live in.
pub(crate) const DIRECTORY: &str = "extensions";
/// The most bytes a codec module may be.
pub(crate) const MODULE_LIMIT: u64 = 64 << 20;

const TRUST_FILE: &str = "trust.toml";
const CODECS: &str = "codecs";
const PROVIDERS: &str = "providers";
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

/// The folder in `home` that installed provider programs, one envelope each, are read from.
pub fn providers_directory(home: &Home) -> PathBuf {
    home.root().join(DIRECTORY).join(PROVIDERS)
}

/// Signers local policy trusts to publish extensions, what each may publish, and what is revoked.
///
/// Every trusted signer may publish provider-neutral codecs. A codec whose descriptor names a
/// provider also needs its signer to be granted that provider, because it is then served under
/// that provider's identity and policy. Introducing a provider program is a separate grant: a
/// program defines a provider's identity, endpoints and policy, which is more than binding a codec
/// to one.
#[derive(Debug, Clone, Default)]
pub struct ExtensionTrust {
    signers: BTreeMap<String, TrustedSigner>,
    revoked_signers: BTreeSet<String>,
    revoked_digests: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct TrustedSigner {
    key: VerifyingKey,
    providers: BTreeSet<String>,
    programs: BTreeSet<String>,
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
                programs: BTreeSet::new(),
            },
        );
        self
    }

    /// This trust, also letting `signer` introduce provider programs for the provider ids
    /// `programs`. A signer this trust does not name gains nothing.
    #[must_use]
    pub fn with_programs(
        mut self,
        signer: &str,
        programs: impl IntoIterator<Item = String>,
    ) -> Self {
        if let Some(trusted) = self.signers.get_mut(signer) {
            trusted.programs.extend(programs);
        }
        self
    }

    /// This trust, refusing everything `signer` signed.
    #[must_use]
    pub fn with_revoked_signer(mut self, signer: impl Into<String>) -> Self {
        self.revoked_signers.insert(signer.into());
        self
    }

    /// This trust, refusing the extension whose package digest is `digest`, with or without a
    /// `sha256:` prefix.
    #[must_use]
    pub fn with_revoked_digest(mut self, digest: &str) -> Self {
        self.revoked_digests.insert(normalized_digest(digest));
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

    /// Whether `signer` may introduce a provider program for the provider `provider`.
    pub fn may_introduce(&self, signer: &str, provider: &str) -> bool {
        self.signers
            .get(signer)
            .is_some_and(|trusted| trusted.programs.contains(provider))
    }

    /// Whether `signer` is revoked.
    pub fn is_revoked_signer(&self, signer: &str) -> bool {
        self.revoked_signers.contains(signer)
    }

    /// Whether the package digest `digest` is revoked.
    pub fn is_revoked_digest(&self, digest: &str) -> bool {
        self.revoked_digests.contains(&normalized_digest(digest))
    }

    /// The keys and revocations, as the registry verifies programs with them.
    pub(crate) fn program_trust(&self) -> ProgramTrust {
        ProgramTrust {
            trusted_keys: self.keys(),
            revoked_signers: self.revoked_signers.clone(),
            revoked_digests: self.revoked_digests.clone(),
        }
    }
}

/// `digest` without a `sha256:` prefix, in lowercase.
fn normalized_digest(digest: &str) -> String {
    digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .to_ascii_lowercase()
}

/// What kind of file an [`InstalledExtension`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    /// The trust root. It is listed only when it cannot be read, since then nothing is trusted.
    TrustRoot,
    /// A WebAssembly pack codec.
    Codec,
    /// A declarative provider program.
    Program,
}

/// Whether an installed extension runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionStatus {
    /// It was admitted and serves requests.
    Active,
    /// It was refused, and does not run.
    Refused,
}

/// An extension found in the data directory, and whether it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledExtension {
    /// What kind of file it is.
    pub kind: ExtensionKind,
    /// The envelope document, or the folder or trust root that could not be read.
    pub path: PathBuf,
    /// The extension ID, when the envelope could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The extension version, when the envelope could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The signer the envelope names, when it could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    /// The package digest the envelope declares, when it could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Whether it runs.
    pub status: ExtensionStatus,
    /// Why it was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl InstalledExtension {
    /// An extension that could not be read far enough to name itself.
    pub(crate) fn unreadable(kind: ExtensionKind, path: PathBuf, reason: &RegistryError) -> Self {
        Self {
            kind,
            path,
            id: None,
            version: None,
            signer: None,
            digest: None,
            status: ExtensionStatus::Refused,
            reason: Some(reason.to_string()),
        }
    }

    /// The extension `envelope` describes, found at `path`, admitted when `refusal` is `None`.
    pub(crate) fn from_envelope<T>(
        kind: ExtensionKind,
        path: PathBuf,
        envelope: &ExtensionEnvelope<T>,
        refusal: Option<&RegistryError>,
    ) -> Self {
        Self {
            kind,
            path,
            id: Some(envelope.id.clone()),
            version: Some(envelope.version.clone()),
            signer: Some(envelope.signer.clone()),
            digest: Some(envelope.package_digest.clone()),
            status: if refusal.is_some() {
                ExtensionStatus::Refused
            } else {
                ExtensionStatus::Active
            },
            reason: refusal.map(ToString::to_string),
        }
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
    #[serde(default)]
    revoked: RevokedDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignerDocument {
    id: String,
    key: String,
    #[serde(default)]
    providers: Vec<String>,
    #[serde(default)]
    programs: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokedDocument {
    #[serde(default)]
    signers: Vec<String>,
    #[serde(default)]
    digests: Vec<String>,
}

/// An installed codec's envelope, with its module bytes as the payload.
pub(crate) struct InstalledCodec {
    /// The envelope document it was read from.
    pub(crate) path: PathBuf,
    pub(crate) envelope: ExtensionEnvelope<Vec<u8>>,
}

/// A document found beneath the extensions directory, or why it could not be read.
pub(crate) struct Found<T> {
    /// The document, or the folder that could not be listed.
    pub(crate) path: PathBuf,
    pub(crate) item: Result<T, RegistryError>,
}

/// Everything installed beneath an extensions directory, each part read on its own.
pub(crate) struct Installed {
    /// The trust root, or why it cannot be read, in which case nothing installed is trusted.
    pub(crate) trust: Result<ExtensionTrust, RegistryError>,
    /// Codecs in envelope file name order.
    pub(crate) codecs: Vec<Found<InstalledCodec>>,
    /// Provider program envelope documents in file name order.
    pub(crate) programs: Vec<Found<String>>,
}

/// Reads the trust root, every codec envelope and every program envelope beneath `directory`. A
/// missing directory, trust root or folder is simply empty; anything else that fails is reported
/// in its place.
pub(crate) fn read(directory: &Path) -> Installed {
    let codecs = documents(&directory.join(CODECS), |path| read_codec(path.clone()));
    let programs = documents(&directory.join(PROVIDERS), |path| read_program(path));
    Installed {
        trust: read_trust(directory),
        codecs,
        programs,
    }
}

/// `read` applied to each `.toml` document in `folder`, in file name order.
fn documents<T>(
    folder: &Path,
    read: impl Fn(&PathBuf) -> Result<T, RegistryError>,
) -> Vec<Found<T>> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            return vec![Found {
                path: folder.to_path_buf(),
                item: Err(invalid(folder, error)),
            }];
        }
    };
    let mut paths = Vec::new();
    let mut found = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "toml")
                {
                    paths.push(path);
                }
            }
            Err(error) => found.push(Found {
                path: folder.to_path_buf(),
                item: Err(invalid(folder, error)),
            }),
        }
    }
    paths.sort();
    found.extend(paths.into_iter().map(|path| Found {
        item: read(&path),
        path,
    }));
    found
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
    let text = read_text(&path)?;
    let document: CodecDocument = toml::from_str(&text).map_err(|error| invalid(&path, error))?;
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

/// The provider program envelope document at `path`, as text.
pub(crate) fn read_program(path: &Path) -> Result<String, RegistryError> {
    read_text(path)
}

fn read_text(path: &Path) -> Result<String, RegistryError> {
    let bytes = read_limited(path, DOCUMENT_LIMIT)?
        .ok_or_else(|| invalid(path, "the envelope does not exist"))?;
    String::from_utf8(bytes).map_err(|error| invalid(path, error))
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
        if let Some(program) = signer.programs.iter().find(|id| !is_provider_id(id)) {
            return Err(invalid(
                path,
                format!(
                    "signer {:?} grants {program:?}, which is not a provider id",
                    signer.id
                ),
            ));
        }
        let id = signer.id.clone();
        trust = trust
            .with_signer(signer.id, key, signer.providers)
            .with_programs(&id, signer.programs);
    }
    for digest in &document.revoked.digests {
        let normalized = normalized_digest(digest);
        if decode_bytes(&normalized).is_none() {
            return Err(invalid(
                path,
                format!("revoked digest {digest:?} is not a 64-character hexadecimal SHA-256"),
            ));
        }
        trust.revoked_digests.insert(normalized);
    }
    trust.revoked_signers.extend(document.revoked.signers);
    Ok(trust)
}

/// Whether `id` is shaped like a provider id: lowercase ASCII letters, digits and hyphens.
fn is_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use msbe_provider_api::{SigningKey, hex};

    use super::{parse_trust, read};
    use crate::RegistryError;

    fn key() -> String {
        hex(SigningKey::from_bytes(&[3; 32]).verifying_key().as_bytes())
    }

    #[test]
    fn a_trust_root_grants_programs_per_signer_and_revokes_signers_and_digests() {
        let document = format!(
            "[[signer]]\nid = \"one\"\nkey = \"{}\"\nprograms = [\"catalog\"]\n\n[[signer]]\nid = \"two\"\nkey = \"{}\"\nproviders = [\"catalog\"]\n\n[revoked]\nsigners = [\"old\"]\ndigests = [\"sha256:{}\"]\n",
            key(),
            key(),
            "AB".repeat(32)
        );
        let trust = parse_trust(Path::new("trust.toml"), document.as_bytes()).unwrap();
        assert!(trust.may_introduce("one", "catalog"));
        assert!(!trust.may_introduce("two", "catalog"));
        assert!(!trust.may_introduce("one", "other"));
        assert!(trust.may_publish_for("two", "catalog"));
        assert!(!trust.may_publish_for("one", "catalog"));
        assert!(trust.is_revoked_signer("old"));
        assert!(trust.is_revoked_digest(&"ab".repeat(32)));
        assert!(!trust.is_revoked_digest(&"cd".repeat(32)));

        for broken in [
            document.replace("programs = [\"catalog\"]", "programs = [\"Catalog\"]"),
            document.replace(&format!("sha256:{}", "AB".repeat(32)), "not-a-digest"),
            document.replace("id = \"two\"", "id = \"one\""),
            format!("{document}unknown = true\n"),
        ] {
            assert!(
                matches!(
                    parse_trust(Path::new("trust.toml"), broken.as_bytes()),
                    Err(RegistryError::InstalledExtension { .. })
                ),
                "{broken}"
            );
        }
    }

    #[test]
    fn each_installed_document_is_read_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let providers = dir.path().join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(providers.join("b.toml"), "second").unwrap();
        std::fs::write(providers.join("a.toml"), "first").unwrap();
        std::fs::write(providers.join("notes.txt"), "ignored").unwrap();
        std::fs::write(providers.join("c.toml"), vec![0xff, 0xfe]).unwrap();
        std::fs::write(dir.path().join("trust.toml"), "[[signer]]\n").unwrap();

        let installed = read(dir.path());
        assert!(installed.trust.is_err());
        assert!(installed.codecs.is_empty());
        let programs: Vec<(String, bool)> = installed
            .programs
            .iter()
            .map(|found| {
                (
                    found
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    found.item.is_ok(),
                )
            })
            .collect();
        assert_eq!(
            programs,
            [
                ("a.toml".to_owned(), true),
                ("b.toml".to_owned(), true),
                ("c.toml".to_owned(), false),
            ]
        );
    }
}
