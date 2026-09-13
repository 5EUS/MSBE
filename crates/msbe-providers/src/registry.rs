//! The fail-closed mapping from provider manifests to reviewed adapters.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use msbe_core::{config::Home, instance::ExtensionPin};
use msbe_fsops::Digest;
use msbe_provider_api::{
    Adapter, AdapterError, Catalog, ExtensionCapability, ExtensionEnvelope, ExtensionProvide,
    HostApiRange, HttpClient, ManifestError, Overlay, OverlayError, PackCodec, PackCodecDescriptor,
    PackCodecError, PackCodecRegistration, PackInput, ProgramError, Provider, ProviderProgram,
    ProviderProgramEnvelope, Registration, SigningKey, Target, VerifyingKey,
    WasmPackCodecRegistration,
    model::{Request, SearchResult},
    resolve::{Adapters, ResolveError},
};
use msbe_wasm_codec::WasmPackCodec;
use thiserror::Error;

use crate::{ExtensionTrust, installed, runtime};

const NATIVE_HOST_API_VERSION: u32 = 1;
/// The signer every extension compiled into or embedded in this build is pinned with.
const BUILD_SIGNER: &str = "msbe-build";

/// The adapters MSBE ships. A new provider is a crate beside these and one line here.
pub const BUILTIN: &[Registration] = &[
    msbe_provider_modrinth::REGISTRATION,
    msbe_provider_local::REGISTRATION,
];

const DIRECT_PROGRAM: &str = r#"
runtime = "direct-url-v1"

[provider]
schema = 1
id = "url"
name = "Direct URL"

[provider.source]
type = "https_url"

[provider.acquisition]
type = "direct_https"

[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = ""
ack_required = false
"#;

/// A user-entered source, routed to its provider and parsed by that provider's adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    /// The provider id.
    pub provider: String,
    /// What the source asks the provider for.
    pub request: Request,
}

/// Trust inputs supplied by the registry root for declarative provider programs.
#[derive(Debug, Clone, Default)]
pub struct ProgramTrust {
    /// Signer identities and Ed25519 verifying keys allowed to introduce programs.
    pub trusted_keys: BTreeMap<String, VerifyingKey>,
    /// Signer identities no longer permitted to introduce programs.
    pub revoked_signers: BTreeSet<String>,
    /// Content digests that are no longer permitted.
    pub revoked_digests: BTreeSet<String>,
}

/// Validated provider manifests, the adapters that serve them, and the overlay those adapters
/// ship.
#[derive(Debug)]
pub struct Providers {
    catalog: Catalog,
    adapters: BTreeMap<String, Box<dyn Adapter>>,
    codecs: CodecTable,
    extensions: Vec<ExtensionPin>,
    overlay: Overlay,
}

#[derive(Debug)]
struct RegisteredCodec {
    provider: Option<String>,
    codec: Box<dyn PackCodec>,
}

impl Providers {
    /// Adds a trusted WebAssembly pack codec to this registry.
    ///
    /// A codec whose descriptor names a provider is served under that provider's policy, and is
    /// admitted only when the provider is registered and `trust` lets the envelope's signer publish
    /// codecs for it. Installed codecs are not native build pins, so they never change which
    /// native bundles this build accepts.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the envelope is untrusted, incompatible or invalid, its
    /// provider is unknown or not granted to its signer, or its ID or a detection hint is taken.
    pub fn register_wasm_codec(
        &mut self,
        envelope: &ExtensionEnvelope<Vec<u8>>,
        trust: &ExtensionTrust,
    ) -> Result<(), RegistryError> {
        let codec = WasmPackCodec::load_signed(envelope, &trust.keys())?;
        let descriptor = codec.descriptor();
        if let Some(provider) = &descriptor.provider {
            self.catalog.provider(provider)?;
            if !trust.may_publish_for(&envelope.signer, provider) {
                return Err(RegistryError::WasmCodecProviderNotGranted {
                    codec: descriptor.id.clone(),
                    provider: provider.clone(),
                    signer: envelope.signer.clone(),
                });
            }
        }
        self.admit_wasm_codec(codec)
    }

    /// Adds a WebAssembly codec that `provider`'s registration ships in this build. Like a native
    /// codec, it may name only that provider, and it is pinned as part of the build.
    fn register_shipped_codec(
        &mut self,
        provider: &str,
        shipped: &WasmPackCodecRegistration,
    ) -> Result<(), RegistryError> {
        let codec = WasmPackCodec::load(shipped.module)?;
        let descriptor = codec.descriptor();
        if descriptor.id != shipped.id {
            return Err(RegistryError::MismatchedCodec {
                registration: shipped.id.to_owned(),
                descriptor: descriptor.id.clone(),
            });
        }
        if descriptor
            .provider
            .as_deref()
            .is_some_and(|claimed| claimed != provider)
        {
            return Err(RegistryError::MismatchedCodecProvider {
                codec: descriptor.id.clone(),
                registration: provider.to_owned(),
                descriptor: descriptor.provider.clone(),
            });
        }
        let pin = ExtensionPin {
            id: shipped.id.to_owned(),
            version: shipped.version.to_owned(),
            digest: Digest::of_bytes(shipped.module),
            host_api_minimum: NATIVE_HOST_API_VERSION,
            host_api_maximum: NATIVE_HOST_API_VERSION,
            signer: BUILD_SIGNER.to_owned(),
        };
        self.admit_wasm_codec(codec)?;
        self.extensions.push(pin);
        Ok(())
    }

    /// Adds a loaded WebAssembly codec under the provider its descriptor names, once its ID and
    /// detection hints are free.
    fn admit_wasm_codec(&mut self, codec: WasmPackCodec) -> Result<(), RegistryError> {
        let descriptor = codec.descriptor();
        self.codecs.claim(descriptor)?;
        let (id, provider) = (descriptor.id.clone(), descriptor.provider.clone());
        self.codecs.codecs.insert(
            id,
            RegisteredCodec {
                provider,
                codec: Box::new(codec),
            },
        );
        Ok(())
    }

    /// The providers MSBE ships, plus the WebAssembly pack codecs installed in `home` that its
    /// `extensions/trust.toml` trusts (`docs/18-wasm-extensions.md` §18.3).
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::InstalledExtension`] for an unreadable or malformed trust root or
    /// envelope, and [`RegistryError::InstalledCodec`] naming the envelope of a refused codec. One
    /// refused codec refuses them all, so nothing runs with trust the user did not intend.
    pub fn installed(home: &Home) -> Result<Self, RegistryError> {
        let mut providers = Self::builtins()?;
        let found = installed::read(&home.root().join(installed::DIRECTORY))?;
        for codec in found.codecs {
            providers
                .register_wasm_codec(&codec.envelope, &found.trust)
                .map_err(|source| RegistryError::InstalledCodec {
                    path: codec.path,
                    source: Box::new(source),
                })?;
        }
        Ok(providers)
    }

    /// The providers MSBE ships.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] if a built-in manifest or overlay entry is invalid. This
    /// indicates a build error in MSBE rather than user-provided input.
    pub fn builtins() -> Result<Self, RegistryError> {
        let direct = builtin_direct_program()?;
        let trust = ProgramTrust {
            trusted_keys: [(
                "msbe-builtin".to_owned(),
                builtin_signing_key().verifying_key(),
            )]
            .into(),
            revoked_signers: BTreeSet::new(),
            revoked_digests: BTreeSet::new(),
        };
        Self::new_with_programs(BUILTIN, &[], &[&direct], &trust)
    }

    /// Registers `registrations`, plus `manifests` no compiled adapter serves, such as ones a
    /// registry distributes. A source such a manifest recognizes is refused, never interpreted
    /// generically.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] if a manifest or overlay entry is invalid, or a registration
    /// builds an adapter for a provider other than its manifest's.
    pub fn new(registrations: &[Registration], manifests: &[&str]) -> Result<Self, RegistryError> {
        Self::new_with_programs(registrations, manifests, &[], &ProgramTrust::default())
    }

    /// Registers reviewed adapters and trusted declarative provider programs.
    ///
    /// Programs are structurally validated, then checked against the caller-supplied signer
    /// allowlist and revocation set before their reviewed runtime is selected.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] for an invalid manifest, overlay, codec or program, an untrusted
    /// or revoked program, or a provider served twice.
    pub fn new_with_programs(
        registrations: &[Registration],
        manifests: &[&str],
        program_documents: &[&str],
        trust: &ProgramTrust,
    ) -> Result<Self, RegistryError> {
        let programs: Vec<ProviderProgramEnvelope> = program_documents
            .iter()
            .map(|document| ProviderProgramEnvelope::from_toml(document))
            .collect::<Result<_, _>>()?;
        for envelope in &programs {
            if trust.revoked_signers.contains(&envelope.0.signer) {
                return Err(RegistryError::RevokedProgramSigner(
                    envelope.0.signer.clone(),
                ));
            }
            if trust.revoked_digests.contains(&envelope.0.package_digest) {
                return Err(RegistryError::RevokedProgram(
                    envelope.0.package_digest.clone(),
                ));
            }
            envelope.verify(&trust.trusted_keys)?;
        }
        let program_manifests: Vec<String> = programs
            .iter()
            .map(|envelope| {
                toml::to_string(&envelope.0.payload.provider).map_err(|error| {
                    RegistryError::Program(ProgramError::Canonical(error.to_string()))
                })
            })
            .collect::<Result<_, _>>()?;
        let documents: Vec<&str> = registrations
            .iter()
            .map(|registration| registration.manifest)
            .chain(manifests.iter().copied())
            .chain(program_manifests.iter().map(String::as_str))
            .collect();
        let catalog = Catalog::from_toml(&documents)?;
        let mut adapters = BTreeMap::new();
        let mut codecs = CodecTable::default();
        let mut extensions = Vec::new();
        let mut overlay = Vec::new();
        for registration in registrations {
            if registration.exception_reason.trim().is_empty() {
                return Err(RegistryError::MissingNativeException(
                    registration.id.to_owned(),
                ));
            }
            if registration.identity.id != registration.id {
                return Err(RegistryError::MismatchedNativeIdentity {
                    registration: registration.id.to_owned(),
                    identity: registration.identity.id.to_owned(),
                });
            }
            let manifest: toml::Value = toml::from_str(registration.manifest)
                .map_err(|error| RegistryError::NativeCanonical(error.to_string()))?;
            extensions.push(native_pin(&registration.identity, manifest)?);
            let provider = catalog.provider(registration.id)?;
            let adapter = (registration.build)(provider)?;
            if adapter.id() != provider.id {
                return Err(RegistryError::MismatchedAdapter {
                    manifest: provider.id.clone(),
                    adapter: adapter.id().to_owned(),
                });
            }
            adapters.insert(provider.id.clone(), adapter);
            for codec_registration in registration.pack_codecs {
                extensions.push(codecs.register(&provider.id, codec_registration)?);
            }
            overlay.extend_from_slice(registration.overlay);
        }
        for envelope in programs {
            let provider = catalog.provider(&envelope.0.payload.provider.id)?;
            let adapter = runtime::build(envelope.0.payload);
            if adapter.id() != provider.id {
                return Err(RegistryError::MismatchedAdapter {
                    manifest: provider.id.clone(),
                    adapter: adapter.id().to_owned(),
                });
            }
            if adapters.insert(provider.id.clone(), adapter).is_some() {
                return Err(RegistryError::DuplicateProgramAdapter(provider.id.clone()));
            }
        }
        let mut providers = Self {
            catalog,
            adapters,
            codecs,
            extensions,
            overlay: Overlay::from_toml(&overlay)?,
        };
        for registration in registrations {
            for shipped in registration.wasm_pack_codecs {
                providers.register_shipped_codec(registration.id, shipped)?;
            }
        }
        Ok(providers)
    }

    /// The overlay entries every registered adapter ships.
    pub const fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    /// Routes a user-entered source to its provider and parses it with that provider's adapter.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when no manifest recognizes the source, its provider has no
    /// adapter or is prohibited by policy, or the adapter rejects the reference.
    pub fn request(&self, raw: &str) -> Result<Routed, RegistryError> {
        let source = self.catalog.source(raw)?;
        let adapter = self.permitted(source.provider())?;
        Ok(Routed {
            provider: source.provider().id.clone(),
            request: adapter.request(source.reference())?,
        })
    }

    /// The adapter for provider `id`, once its declared policy has been checked.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the provider is unknown, has no adapter, or is prohibited
    /// by policy.
    pub fn adapter(&self, id: &str) -> Result<&dyn Adapter, RegistryError> {
        self.permitted(self.catalog.provider(id)?)
    }

    /// Descriptors for policy-permitted pack codecs, ordered by codec ID.
    pub fn pack_codecs(&self) -> Vec<&PackCodecDescriptor> {
        self.codecs
            .codecs
            .values()
            .filter(|registered| {
                registered.provider.as_ref().is_none_or(|provider_id| {
                    self.catalog
                        .provider(provider_id)
                        .is_ok_and(|provider| authorize(provider).is_ok())
                })
            })
            .map(|registered| registered.codec.descriptor())
            .collect()
    }

    /// Reviewed native extension identities pinned by this MSBE build.
    #[must_use]
    pub fn extension_pins(&self) -> Vec<ExtensionPin> {
        self.extensions.clone()
    }

    /// Whether `pins` name precisely the native extensions reviewed into this build.
    ///
    /// Empty pins are accepted for native bundles produced before extension pinning was added.
    #[must_use]
    pub fn accepts_extension_pins(&self, pins: &[ExtensionPin]) -> bool {
        if pins.is_empty() {
            return true;
        }
        let mut expected = self.extension_pins();
        expected.sort_by(|left, right| left.id.cmp(&right.id));
        expected == pins
    }

    /// Looks up a reviewed pack codec after enforcing its provider's policy.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the codec is unknown or its provider is prohibited.
    pub fn pack_codec(&self, id: &str) -> Result<&dyn PackCodec, RegistryError> {
        let registered = self
            .codecs
            .codecs
            .get(id)
            .ok_or_else(|| RegistryError::UnknownCodec(id.to_owned()))?;
        if let Some(provider_id) = &registered.provider {
            authorize(self.catalog.provider(provider_id)?)?;
        }
        Ok(registered.codec.as_ref())
    }

    /// Detects a pack format using every policy-permitted codec.
    ///
    /// Each codec receives the same bounded, immutable input. A tie at the highest non-zero
    /// confidence is rejected rather than resolved by registration order.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when seeking or probing fails, or detection is ambiguous.
    pub fn detect_pack_codec(
        &self,
        input: &dyn PackInput,
    ) -> Result<Option<&dyn PackCodec>, RegistryError> {
        let mut best: Option<(&dyn PackCodec, u8)> = None;
        let mut tied = Vec::new();
        for descriptor in self.pack_codecs() {
            let codec = self.pack_codec(&descriptor.id)?;
            let confidence = codec.probe(input)?.confidence;
            if confidence == 0 {
                continue;
            }
            match best {
                Some((_, best_confidence)) if confidence < best_confidence => {}
                Some((_, best_confidence)) if confidence == best_confidence => {
                    tied.push(descriptor.id.clone());
                }
                _ => {
                    best = Some((codec, confidence));
                    tied.clear();
                    tied.push(descriptor.id.clone());
                }
            }
        }
        if tied.len() > 1 {
            return Err(RegistryError::AmbiguousCodec(tied));
        }
        Ok(best.map(|(codec, _)| codec))
    }

    /// The ids of permitted providers that can search, in id order.
    pub fn searchable(&self) -> Vec<&str> {
        self.adapters
            .iter()
            .filter(|(id, adapter)| adapter.as_search().is_some() && self.adapter(id).is_ok())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Searches one provider and returns provider-neutral project records.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the provider is unavailable, prohibited by policy, does
    /// not support search, or its adapter cannot complete the request.
    pub fn search(
        &self,
        id: &str,
        http: &dyn HttpClient,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, RegistryError> {
        let search = self
            .adapter(id)?
            .as_search()
            .ok_or_else(|| RegistryError::SearchUnavailable(id.to_owned()))?;
        Ok(search.search(http, query, target, limit)?)
    }

    fn permitted(&self, provider: &Provider) -> Result<&dyn Adapter, RegistryError> {
        authorize(provider)?;
        self.adapters
            .get(&provider.id)
            .map(Box::as_ref)
            .ok_or_else(|| RegistryError::UnavailableAdapter(provider.id.clone()))
    }
}

fn builtin_direct_program() -> Result<String, RegistryError> {
    let program: ProviderProgram = toml::from_str(DIRECT_PROGRAM)
        .map_err(|error| RegistryError::Program(ProgramError::Parse(error.to_string())))?;
    let mut envelope = ExtensionEnvelope {
        schema: 1,
        package_digest: ProviderProgramEnvelope::digest_for(&program)?,
        id: "msbe-direct-url".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        provides: vec![ExtensionProvide::ProviderProgramV1],
        host_api: HostApiRange {
            minimum: 1,
            maximum: 1,
        },
        capabilities: vec![ExtensionCapability::Network],
        signer: "msbe-builtin".to_owned(),
        signature: "00".repeat(64),
        payload: program,
    };
    envelope
        .sign(&builtin_signing_key())
        .map_err(ProgramError::from)?;
    toml::to_string(&ProviderProgramEnvelope(envelope))
        .map_err(|error| RegistryError::Program(ProgramError::Canonical(error.to_string())))
}

fn builtin_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[
        90, 80, 20, 229, 17, 66, 33, 121, 94, 153, 27, 192, 63, 18, 74, 155, 222, 39, 50, 195, 70,
        164, 31, 88, 6, 183, 11, 245, 128, 219, 44, 101,
    ])
}

impl Adapters for Providers {
    fn lookup(&self, provider: &str) -> Result<&dyn Adapter, ResolveError> {
        self.adapter(provider)
            .map_err(|error| ResolveError::Unavailable {
                provider: provider.to_owned(),
                reason: error.to_string(),
            })
    }
}

fn authorize(provider: &Provider) -> Result<(), RegistryError> {
    if provider.policy.requires_auth {
        return Err(RegistryError::AuthenticationRequired(provider.id.clone()));
    }
    if provider.policy.ack_required {
        return Err(RegistryError::AcknowledgementRequired {
            provider: provider.id.clone(),
            terms: provider.policy.tos_url.clone(),
        });
    }
    Ok(())
}

/// Registered codecs, with the detection hints each has claimed.
#[derive(Debug, Default)]
struct CodecTable {
    codecs: BTreeMap<String, RegisteredCodec>,
    extensions: BTreeMap<String, String>,
    media_types: BTreeMap<String, String>,
}

impl CodecTable {
    /// Builds and validates one codec for `provider`, rejecting mismatched or duplicate IDs and
    /// claimed hints.
    fn register(
        &mut self,
        provider: &str,
        registration: &PackCodecRegistration,
    ) -> Result<ExtensionPin, RegistryError> {
        let codec = (registration.build)()?;
        let descriptor = codec.descriptor();
        descriptor.validate()?;
        if descriptor.id != registration.id {
            return Err(RegistryError::MismatchedCodec {
                registration: registration.id.to_owned(),
                descriptor: descriptor.id.clone(),
            });
        }
        if registration.identity.id != registration.id {
            return Err(RegistryError::MismatchedNativeIdentity {
                registration: registration.id.to_owned(),
                identity: registration.identity.id.to_owned(),
            });
        }
        if descriptor
            .provider
            .as_deref()
            .is_some_and(|id| id != provider)
        {
            return Err(RegistryError::MismatchedCodecProvider {
                codec: descriptor.id.clone(),
                registration: provider.to_owned(),
                descriptor: descriptor.provider.clone(),
            });
        }
        let extension = native_pin(&registration.identity, descriptor)?;
        self.claim(descriptor)?;
        self.codecs.insert(
            descriptor.id.clone(),
            RegisteredCodec {
                provider: Some(provider.to_owned()),
                codec,
            },
        );
        Ok(extension)
    }

    /// Claims `descriptor`'s ID and detection hints, refusing any that another codec holds or
    /// that the descriptor repeats. Nothing is claimed unless all of it is free.
    fn claim(&mut self, descriptor: &PackCodecDescriptor) -> Result<(), RegistryError> {
        if self.codecs.contains_key(&descriptor.id) {
            return Err(RegistryError::DuplicateCodec(descriptor.id.clone()));
        }
        for (claimed, hints) in [
            (&self.extensions, &descriptor.extensions),
            (&self.media_types, &descriptor.media_types),
        ] {
            let mut own = BTreeSet::new();
            for hint in hints {
                let normalized = hint.to_ascii_lowercase();
                let holder = claimed
                    .get(&normalized)
                    .cloned()
                    .or_else(|| (!own.insert(normalized.clone())).then(|| descriptor.id.clone()));
                if let Some(first) = holder {
                    return Err(RegistryError::DuplicateCodecHint {
                        hint: normalized,
                        first,
                        second: descriptor.id.clone(),
                    });
                }
            }
        }
        for hint in &descriptor.extensions {
            self.extensions
                .insert(hint.to_ascii_lowercase(), descriptor.id.clone());
        }
        for hint in &descriptor.media_types {
            self.media_types
                .insert(hint.to_ascii_lowercase(), descriptor.id.clone());
        }
        Ok(())
    }
}

fn native_pin(
    identity: &msbe_core::instance::NativeExtensionIdentity,
    source: impl serde::Serialize,
) -> Result<ExtensionPin, RegistryError> {
    for (field, value) in [
        ("extension id", identity.id),
        ("extension version", identity.version),
        ("extension signer", identity.signer),
    ] {
        if value.trim().is_empty() || !value.is_ascii() {
            return Err(RegistryError::InvalidNativeIdentity {
                field,
                value: value.to_owned(),
            });
        }
    }
    if identity.host_api_minimum > identity.host_api_maximum
        || !(identity.host_api_minimum..=identity.host_api_maximum)
            .contains(&NATIVE_HOST_API_VERSION)
    {
        return Err(RegistryError::UnsupportedNativeHostApi {
            id: identity.id.to_owned(),
            minimum: identity.host_api_minimum,
            maximum: identity.host_api_maximum,
        });
    }
    let source: toml::Value = toml::from_str(
        &toml::to_string(&source)
            .map_err(|error| RegistryError::NativeCanonical(error.to_string()))?,
    )
    .map_err(|error| RegistryError::NativeCanonical(error.to_string()))?;
    let bytes = serde_json::to_vec(&source)
        .map_err(|error| RegistryError::NativeCanonical(error.to_string()))?;
    Ok(ExtensionPin {
        id: identity.id.to_owned(),
        version: identity.version.to_owned(),
        digest: Digest::of_bytes(&bytes),
        host_api_minimum: identity.host_api_minimum,
        host_api_maximum: identity.host_api_maximum,
        signer: identity.signer.to_owned(),
    })
}

/// Why a provider could not be used through the reviewed adapter registry.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// A WASM codec binds to a provider its signer is not trusted to publish codecs for.
    #[error(
        "WASM codec {codec:?} binds to provider {provider:?}, which signer {signer:?} is not trusted to publish codecs for"
    )]
    WasmCodecProviderNotGranted {
        /// Codec ID.
        codec: String,
        /// The provider its descriptor names.
        provider: String,
        /// The envelope's signer.
        signer: String,
    },
    /// An installed extension file could not be read or is malformed.
    #[error("installed extension {}: {reason}", .path.display())]
    InstalledExtension {
        /// The file or directory.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// An installed codec was refused.
    #[error("installed codec {}: {source}", .path.display())]
    InstalledCodec {
        /// Its envelope document.
        path: PathBuf,
        /// Why it was refused.
        #[source]
        source: Box<RegistryError>,
    },
    /// Native registration identity differs from the provider or codec it serves.
    #[error("native extension identity {identity:?} does not match registration {registration:?}")]
    MismatchedNativeIdentity {
        /// Provider or codec registration ID.
        registration: String,
        /// ID declared by native identity metadata.
        identity: String,
    },
    /// Native identity contains an empty or non-ASCII field.
    #[error("invalid native {field} {value:?}")]
    InvalidNativeIdentity {
        /// Identity field that failed validation.
        field: &'static str,
        /// Rejected identity value.
        value: String,
    },
    /// The compiled extension cannot run against this host API.
    #[error("native extension {id:?} supports host API {minimum}..={maximum}, not this build")]
    UnsupportedNativeHostApi {
        /// Extension ID.
        id: String,
        /// Lowest supported host API.
        minimum: u32,
        /// Highest supported host API.
        maximum: u32,
    },
    /// A built-in manifest or descriptor could not be canonicalized for a pin.
    #[error("cannot canonicalize native extension identity: {0}")]
    NativeCanonical(String),
    /// A native extension did not state why a reviewed runtime cannot serve it.
    #[error("native provider {0:?} must state an exception reason")]
    MissingNativeException(String),
    /// A declarative program failed schema or digest validation.
    #[error(transparent)]
    Program(#[from] ProgramError),
    /// A program signer is not trusted by this registry root.
    #[error("provider program signer {0:?} is revoked")]
    RevokedProgramSigner(String),
    /// A program digest is revoked by this registry root.
    #[error("provider program digest {0:?} is revoked")]
    RevokedProgram(String),
    /// A reviewed runtime could not be constructed from a valid program.
    #[error("provider program runtime failed: {0}")]
    ProgramRuntime(String),
    /// A native registration and a program both attempted to serve one provider.
    #[error("provider {0:?} is served by both a native registration and a program")]
    DuplicateProgramAdapter(String),
    /// A manifest is invalid, or no manifest recognizes a source or names a provider.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// An overlay entry an adapter ships is invalid.
    #[error(transparent)]
    Overlay(#[from] OverlayError),
    /// The manifest names a provider without a reviewed adapter in this MSBE release.
    #[error("provider {0:?} is declared but has no reviewed adapter in this MSBE release")]
    UnavailableAdapter(String),
    /// A registration built an adapter for a different provider than its manifest declares.
    #[error("the {manifest:?} manifest is registered with an adapter for {adapter:?}")]
    MismatchedAdapter {
        /// The provider the manifest declares.
        manifest: String,
        /// The provider the adapter serves.
        adapter: String,
    },
    /// A codec registration and its descriptor use different IDs.
    #[error("pack codec registration {registration:?} built a descriptor for {descriptor:?}")]
    MismatchedCodec {
        /// ID in the registration.
        registration: String,
        /// ID in the descriptor.
        descriptor: String,
    },
    /// A codec descriptor claims another provider.
    #[error(
        "pack codec {codec:?} is registered by {registration:?} but claims provider {descriptor:?}"
    )]
    MismatchedCodecProvider {
        /// Codec ID.
        codec: String,
        /// Provider registration containing it.
        registration: String,
        /// Provider claimed by the descriptor.
        descriptor: Option<String>,
    },
    /// Two reviewed registrations use the same codec ID.
    #[error("duplicate pack codec id {0:?}")]
    DuplicateCodec(String),
    /// Two codecs claim the same detection hint.
    #[error("pack codec hint {hint:?} is claimed by both {first:?} and {second:?}")]
    DuplicateCodecHint {
        /// Conflicting extension or media type.
        hint: String,
        /// First codec ID.
        first: String,
        /// Second codec ID.
        second: String,
    },
    /// No reviewed codec has this ID.
    #[error("unknown pack codec {0:?}")]
    UnknownCodec(String),
    /// More than one codec matched with the same confidence.
    #[error("pack format is ambiguous between codecs: {0:?}")]
    AmbiguousCodec(Vec<String>),
    /// A pack codec descriptor, probe or operation failed.
    #[error(transparent)]
    PackCodec(#[from] PackCodecError),
    /// The adapter does not support search.
    #[error("provider {0:?} does not support search")]
    SearchUnavailable(String),
    /// The provider requires an authentication flow this release does not implement.
    #[error("provider {0:?} requires authentication, which is not implemented in this release")]
    AuthenticationRequired(String),
    /// The provider requires a persisted terms acknowledgement this release does not implement.
    #[error(
        "provider {provider:?} requires acknowledgement of {terms:?}, which is not implemented in this release"
    )]
    AcknowledgementRequired {
        /// The provider id.
        provider: String,
        /// The terms the user would need to acknowledge.
        terms: String,
    },
    /// The provider's adapter rejected a reference or failed a request.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path};

    use msbe_core::{config::Home, instance::NativeExtensionIdentity};
    use msbe_plan_schema::Side;
    use msbe_provider_api::{
        Adapter, AdapterError, ContainerKind, ExtensionCapability, ExtensionEnvelope,
        ExtensionProvide, HostApiRange, HttpClient, HttpError, PackCodec, PackCodecDescriptor,
        PackCodecError, PackCodecRegistration, PackDirections, PackEntry, PackExportContext,
        PackExportPlan, PackImportContext, PackImportPlan, PackInput, PackLayout, PackOptionSchema,
        PackOptions, PackProbe, PackageId, ProgramError, Provider, ProviderProgram,
        ProviderProgramEnvelope, Registration, SigningKey, SupportSet, Target, model::Request,
    };
    use serde_json::json;

    use super::{ExtensionTrust, ProgramTrust, Providers, RegistryError, Routed};

    struct SearchHttp;

    impl HttpClient for SearchHttp {
        fn get(&self, _: &str, _: &[(&str, &str)], _: u64) -> Result<Vec<u8>, HttpError> {
            Ok(serde_json::to_vec(&json!({ "hits": [{
                "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
                "description": "A rendering engine", "downloads": 42,
                "client_side": "required", "server_side": "required"
            }] }))
            .unwrap())
        }

        fn post_json(&self, _: &str, _: &[u8], _: u64) -> Result<Vec<u8>, HttpError> {
            unreachable!("search never posts")
        }

        fn download(&self, _: &str, _: &mut dyn Write, _: u64) -> Result<u64, HttpError> {
            unreachable!("search never downloads")
        }
    }

    const EXAMPLE: &str = r#"
        schema = 1
        id = "example"
        name = "Example"
        [source]
        type = "prefixed"
        prefix = "example:"
        [acquisition]
        type = "direct_https"
        [policy]
        requires_auth = false
        respects_distribution_flag = false
        tos_url = ""
        ack_required = false
    "#;

    #[derive(Debug)]
    struct ExampleAdapter {
        id: &'static str,
    }

    impl Adapter for ExampleAdapter {
        fn id(&self) -> &str {
            self.id
        }

        fn request(&self, reference: &str) -> Result<Request, AdapterError> {
            Ok(Request::Project {
                reference: reference.to_owned(),
                version: None,
            })
        }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "test builder must implement the fallible registration function pointer"
    )]
    fn build_example(_: &Provider) -> Result<Box<dyn Adapter>, msbe_provider_api::ManifestError> {
        Ok(Box::new(ExampleAdapter { id: "example" }))
    }

    #[derive(Debug)]
    struct FakeCodec {
        descriptor: PackCodecDescriptor,
        confidence: u8,
    }

    impl PackCodec for FakeCodec {
        fn descriptor(&self) -> &PackCodecDescriptor {
            &self.descriptor
        }

        fn probe(&self, _: &dyn PackInput) -> Result<PackProbe, PackCodecError> {
            Ok(PackProbe {
                confidence: self.confidence,
                reason: None,
            })
        }

        fn plan_import(
            &self,
            _: &dyn PackInput,
            _: &PackImportContext,
            _: &PackOptions,
        ) -> Result<PackImportPlan, PackCodecError> {
            Err(PackCodecError::UnsupportedDirection("import"))
        }

        fn plan_export(
            &self,
            _: &PackExportContext<'_>,
            _: &PackOptions,
        ) -> Result<PackExportPlan, PackCodecError> {
            Err(PackCodecError::UnsupportedDirection("export"))
        }

        fn layout(&self, _: &PackExportPlan) -> Result<PackLayout, PackCodecError> {
            Err(PackCodecError::UnsupportedDirection("export"))
        }
    }

    #[derive(Debug)]
    struct EmptyInput;

    impl PackInput for EmptyInput {
        fn container(&self) -> ContainerKind {
            ContainerKind::File
        }
        fn entries(&self) -> &[PackEntry] {
            &[]
        }
        fn read(&self, _: &msbe_fsops::RelPath, _: u64) -> Result<Vec<u8>, PackCodecError> {
            Err(PackCodecError::FormatMismatch)
        }
    }

    fn fake_codec(id: &str, extension: &str, confidence: u8) -> Box<dyn PackCodec> {
        Box::new(FakeCodec {
            descriptor: PackCodecDescriptor {
                id: id.to_owned(),
                provider: Some("example".to_owned()),
                name: id.to_owned(),
                extensions: vec![extension.to_owned()],
                media_types: Vec::new(),
                directions: PackDirections {
                    import: true,
                    export: false,
                },
                supported_games: SupportSet::Universal,
                option_schema: PackOptionSchema {
                    schema: 1,
                    presets: Vec::new(),
                    fields: Vec::new(),
                    constraints: Vec::new(),
                },
            },
            confidence,
        })
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "test builder must implement the fallible codec function pointer"
    )]
    fn build_alpha() -> Result<Box<dyn PackCodec>, PackCodecError> {
        Ok(fake_codec("alpha", "alpha", 80))
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "test builder must implement the fallible codec function pointer"
    )]
    fn build_beta() -> Result<Box<dyn PackCodec>, PackCodecError> {
        Ok(fake_codec("beta", "beta", 80))
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "test builder must implement the fallible codec function pointer"
    )]
    fn build_duplicate_hint() -> Result<Box<dyn PackCodec>, PackCodecError> {
        Ok(fake_codec("beta", "alpha", 70))
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "test builder must implement the fallible codec function pointer"
    )]
    fn build_mismatch() -> Result<Box<dyn PackCodec>, PackCodecError> {
        Ok(fake_codec("other", "other", 90))
    }

    const ALPHA: PackCodecRegistration = PackCodecRegistration {
        id: "alpha",
        identity: NativeExtensionIdentity {
            id: "alpha",
            version: "1.0.0",
            host_api_minimum: 1,
            host_api_maximum: 1,
            signer: "msbe-build",
        },
        build: build_alpha,
    };
    const BETA: PackCodecRegistration = PackCodecRegistration {
        id: "beta",
        identity: NativeExtensionIdentity {
            id: "beta",
            version: "1.0.0",
            host_api_minimum: 1,
            host_api_maximum: 1,
            signer: "msbe-build",
        },
        build: build_beta,
    };
    const DUPLICATE_HINT: PackCodecRegistration = PackCodecRegistration {
        id: "beta",
        identity: NativeExtensionIdentity {
            id: "beta",
            version: "1.0.0",
            host_api_minimum: 1,
            host_api_maximum: 1,
            signer: "msbe-build",
        },
        build: build_duplicate_hint,
    };
    const MISMATCH: PackCodecRegistration = PackCodecRegistration {
        id: "registered",
        identity: NativeExtensionIdentity {
            id: "registered",
            version: "1.0.0",
            host_api_minimum: 1,
            host_api_maximum: 1,
            signer: "msbe-build",
        },
        build: build_mismatch,
    };

    fn registration(codecs: &'static [PackCodecRegistration]) -> Registration {
        Registration {
            id: "example",
            identity: NativeExtensionIdentity {
                id: "example",
                version: "1.0.0",
                host_api_minimum: 1,
                host_api_maximum: 1,
                signer: "msbe-build",
            },
            manifest: EXAMPLE,
            overlay: &[],
            build: build_example,
            pack_codecs: codecs,
            wasm_pack_codecs: &[],
            exception_reason: "Test adapter.",
        }
    }

    fn program(body: &str) -> String {
        let payload = body.replacen("[program]", "", 1).replace("[program.", "[");
        let program: ProviderProgram = toml::from_str(&payload).unwrap();
        let mut envelope = ExtensionEnvelope {
            schema: 1,
            package_digest: ProviderProgramEnvelope::digest_for(&program).unwrap(),
            id: "test-provider".to_owned(),
            version: "1.0.0".to_owned(),
            provides: vec![ExtensionProvide::ProviderProgramV1],
            host_api: HostApiRange {
                minimum: 1,
                maximum: 1,
            },
            capabilities: vec![ExtensionCapability::Network],
            signer: "test-root".to_owned(),
            signature: "00".repeat(64),
            payload: program,
        };
        envelope.sign(&test_signing_key()).unwrap();
        toml::to_string(&ProviderProgramEnvelope(envelope)).unwrap()
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn trust() -> ProgramTrust {
        ProgramTrust {
            trusted_keys: [("test-root".to_owned(), test_signing_key().verifying_key())].into(),
            revoked_signers: std::collections::BTreeSet::default(),
            revoked_digests: std::collections::BTreeSet::default(),
        }
    }

    const PACK_LIST: &[u8] = include_bytes!("../../msbe-wasm-codec/tests/fixtures/pack-list.wasm");

    /// A WASM codec module whose descriptor names `provider`.
    fn wasm_codec(id: &str, provider: Option<&str>) -> Vec<u8> {
        let descriptor = json!({ "ok": {
            "id": id, "provider": provider, "name": id, "extensions": [id], "media_types": [],
            "directions": { "import": true, "export": false },
            "supported_games": { "kind": "universal" }, "option_schema": { "schema": 1 }
        }})
        .to_string();
        let escaped = descriptor.replace('\\', "\\\\").replace('"', "\\\"");
        wat::parse_str(format!(
            r#"(module (memory (export "memory") 1) (data (i32.const 0) "{escaped}")
                (func (export "msbe_abi_version") (result i32) i32.const 1)
                (func (export "msbe_descriptor") (result i64) i64.const {}))"#,
            descriptor.len()
        ))
        .unwrap()
    }

    fn signed_codec(id: &str, module: Vec<u8>) -> ExtensionEnvelope<Vec<u8>> {
        let mut envelope = ExtensionEnvelope {
            schema: 1,
            package_digest: ExtensionEnvelope::package_digest_for(&module).unwrap(),
            id: id.to_owned(),
            version: "1.0.0".to_owned(),
            provides: vec![ExtensionProvide::PackCodecV1],
            host_api: HostApiRange {
                minimum: 1,
                maximum: 1,
            },
            capabilities: Vec::new(),
            signer: "test-root".to_owned(),
            signature: "00".repeat(64),
            payload: module,
        };
        envelope.sign(&test_signing_key()).unwrap();
        envelope
    }

    /// Installs `envelope` in the data directory `home`, as an envelope document beside its module.
    fn install(home: &Path, name: &str, envelope: &ExtensionEnvelope<Vec<u8>>) {
        let codecs = home.join("extensions/codecs");
        std::fs::create_dir_all(&codecs).unwrap();
        std::fs::write(codecs.join(format!("{name}.wasm")), &envelope.payload).unwrap();
        std::fs::write(
            codecs.join(format!("{name}.toml")),
            format!(
                "schema = 1\nid = \"{}\"\nversion = \"{}\"\nmodule = \"{name}.wasm\"\npackage_digest = \"{}\"\nprovides = [\"pack-codec-v1\"]\nhost_api = {{ minimum = 1, maximum = 1 }}\nsigner = \"{}\"\nsignature = \"{}\"\n",
                envelope.id,
                envelope.version,
                envelope.package_digest,
                envelope.signer,
                envelope.signature
            ),
        )
        .unwrap();
    }

    fn trust_root(home: &Path, providers: &str) {
        let key = msbe_provider_api::hex(test_signing_key().verifying_key().as_bytes());
        std::fs::create_dir_all(home.join("extensions")).unwrap();
        std::fs::write(
            home.join("extensions/trust.toml"),
            format!("[[signer]]\nid = \"test-root\"\nkey = \"{key}\"\nproviders = [{providers}]\n"),
        )
        .unwrap();
    }

    #[test]
    fn installed_codecs_load_from_the_data_directory_under_its_trust_root()
    -> Result<(), RegistryError> {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        assert!(
            Providers::installed(&home)?
                .pack_codec("pack-list")
                .is_err()
        );

        install(
            dir.path(),
            "pack-list",
            &signed_codec("pack-list", PACK_LIST.to_vec()),
        );
        assert!(
            matches!(
                Providers::installed(&home),
                Err(RegistryError::InstalledCodec { .. })
            ),
            "a codec from an untrusted signer is refused"
        );

        trust_root(dir.path(), "");
        let providers = Providers::installed(&home)?;
        assert!(
            providers
                .pack_codecs()
                .iter()
                .any(|descriptor| descriptor.id == "pack-list")
        );
        assert_eq!(
            providers.extension_pins(),
            Providers::builtins()?.extension_pins(),
            "installed codecs are not native build pins"
        );

        std::fs::write(
            dir.path().join("extensions/codecs/escape.toml"),
            "schema = 1\nid = \"escape\"\nversion = \"1\"\nmodule = \"../escape.wasm\"\npackage_digest = \"\"\nprovides = []\nhost_api = { minimum = 1, maximum = 1 }\nsigner = \"test-root\"\nsignature = \"\"\n",
        )
        .unwrap();
        assert!(matches!(
            Providers::installed(&home),
            Err(RegistryError::InstalledExtension { .. })
        ));
        Ok(())
    }

    #[test]
    fn provider_bound_wasm_codecs_need_a_signer_granted_that_provider() -> Result<(), RegistryError>
    {
        let key = test_signing_key().verifying_key();
        let bound = signed_codec("bound", wasm_codec("bound", Some("example")));
        let mut providers = Providers::new(&[registration(&[])], &[])?;

        let neutral_only = ExtensionTrust::default().with_signer("test-root", key, Vec::new());
        assert!(matches!(
            providers.register_wasm_codec(&bound, &neutral_only),
            Err(RegistryError::WasmCodecProviderNotGranted { provider, .. }) if provider == "example"
        ));

        let granted = ExtensionTrust::default().with_signer(
            "test-root",
            key,
            ["example".to_owned(), "nobody".to_owned()],
        );
        let orphan = signed_codec("orphan", wasm_codec("orphan", Some("nobody")));
        assert!(matches!(
            providers.register_wasm_codec(&orphan, &granted),
            Err(RegistryError::Manifest(_))
        ));

        providers.register_wasm_codec(&bound, &granted)?;
        assert!(providers.pack_codec("bound").is_ok());
        assert!(matches!(
            providers.register_wasm_codec(&bound, &granted),
            Err(RegistryError::DuplicateCodec(id)) if id == "bound"
        ));
        Ok(())
    }

    #[test]
    fn builtins_route_sources_only_through_their_reviewed_adapters() -> Result<(), RegistryError> {
        let providers = Providers::builtins()?;
        let Routed { provider, request } = providers.request("modrinth:sodium")?;
        assert_eq!(provider, "modrinth");
        assert_eq!(
            request,
            Request::Project {
                reference: "sodium".to_owned(),
                version: None
            }
        );
        let url = providers.request("https://example.test/mod.jar")?;
        assert_eq!(url.provider, "url");
        assert!(matches!(url.request, Request::File(_)));
        assert_eq!(providers.searchable(), ["modrinth"]);
        Ok(())
    }

    #[test]
    fn builtins_load_the_overlay_their_adapters_ship() -> Result<(), RegistryError> {
        let fabric_api = PackageId {
            provider: "modrinth".to_owned(),
            project: "P7dR8mSH".to_owned(),
        };
        assert_eq!(
            Providers::builtins()?
                .overlay()
                .suppliers(&fabric_api)
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn declared_provider_without_an_adapter_is_rejected() {
        let providers = Providers::new(&[], &[EXAMPLE]).unwrap();
        assert!(matches!(
            providers.request("example:mod"),
            Err(RegistryError::UnavailableAdapter(id)) if id == "example"
        ));
    }

    #[test]
    fn acknowledgement_required_providers_are_blocked_before_use() {
        let manifest = EXAMPLE.replace("ack_required = false", "ack_required = true");
        let providers = Providers::new(&[], &[&manifest]).unwrap();
        assert!(matches!(
            providers.request("example:mod"),
            Err(RegistryError::AcknowledgementRequired { .. })
        ));
    }

    #[test]
    fn authentication_required_providers_are_blocked_before_use() {
        let manifest = EXAMPLE.replace("requires_auth = false", "requires_auth = true");
        let providers = Providers::new(&[], &[&manifest]).unwrap();
        assert!(matches!(
            providers.request("example:mod"),
            Err(RegistryError::AuthenticationRequired(id)) if id == "example"
        ));
    }

    #[test]
    fn native_registrations_must_explain_their_exception() {
        let mut registration = registration(&[]);
        registration.exception_reason = " ";
        assert!(matches!(
            Providers::new(&[registration], &[]),
            Err(RegistryError::MissingNativeException(id)) if id == "example"
        ));
    }

    #[test]
    fn native_extension_pins_must_match_the_running_build() -> Result<(), RegistryError> {
        let providers = Providers::new(&[registration(&[])], &[])?;
        let pins = providers.extension_pins();
        assert!(providers.accepts_extension_pins(&pins));

        let mut mismatched = pins;
        let Some(pin) = mismatched.first_mut() else {
            panic!("the native registration pins at least one extension");
        };
        pin.version = "2.0.0".to_owned();
        assert!(!providers.accepts_extension_pins(&mismatched));
        assert!(providers.accepts_extension_pins(&[]));
        Ok(())
    }

    #[test]
    fn programs_fail_closed_for_unknown_signers_and_revocations() {
        let document = program(
            r#"
            [program]
            runtime = "direct-url-v1"
            [program.provider]
            schema = 1
            id = "program-url"
            name = "Program URL"
            [program.provider.source]
            type = "https_url"
            [program.provider.acquisition]
            type = "direct_https"
            [program.provider.policy]
            requires_auth = false
            respects_distribution_flag = false
            tos_url = ""
            ack_required = false
        "#,
        );
        assert!(matches!(
            Providers::new_with_programs(&[], &[], &[&document], &ProgramTrust::default()),
            Err(RegistryError::Program(ProgramError::Envelope(_)))
        ));
        let digest = ProviderProgramEnvelope::from_toml(&document)
            .unwrap()
            .0
            .package_digest;
        let mut revoked = trust();
        revoked.revoked_digests.insert(digest);
        assert!(matches!(
            Providers::new_with_programs(&[], &[], &[&document], &revoked),
            Err(RegistryError::RevokedProgram(_))
        ));
    }

    #[test]
    fn trusted_direct_program_routes_and_parses_a_pinned_url() -> Result<(), RegistryError> {
        let document = program(
            r#"
            [program]
            runtime = "direct-url-v1"
            [program.provider]
            schema = 1
            id = "program-url"
            name = "Program URL"
            [program.provider.source]
            type = "https_url"
            [program.provider.acquisition]
            type = "direct_https"
            [program.provider.policy]
            requires_auth = false
            respects_distribution_flag = false
            tos_url = ""
            ack_required = false
        "#,
        );
        let providers = Providers::new_with_programs(&[], &[], &[&document], &trust())?;
        let routed = providers.request(&format!(
            "https://example.test/mod.jar#sha256={}",
            "a".repeat(64)
        ))?;
        assert_eq!(routed.provider, "program-url");
        assert!(matches!(routed.request, Request::File(_)));
        Ok(())
    }

    #[test]
    fn trusted_catalog_program_maps_a_bounded_search_fixture() -> Result<(), RegistryError> {
        let document = program(
            r#"
            [program]
            runtime = "catalog-v1"
            capabilities = ["search"]
            [program.provider]
            schema = 1
            id = "catalog"
            name = "Catalog"
            [program.provider.source]
            type = "prefixed"
            prefix = "catalog:"
            [program.provider.metadata]
            api_base = "https://api.example.test"
            [program.provider.acquisition]
            type = "direct_https"
            [program.provider.policy]
            requires_auth = false
            respects_distribution_flag = false
            tos_url = ""
            ack_required = false
            [program.routes]
            search = "/search"
            project = "/project/{reference}"
            releases = "/project/{project}/releases"
            [program.mappings]
            search_items = "/hits"
            [program.mappings.project]
            id = "/project_id"
            title = "/title"
            slug = "/slug"
            description = "/description"
            downloads = "/downloads"
            [program.mappings.release]
            id = "/id"
            number = "/number"
            published = "/published"
            files = "/files"
        "#,
        );
        let providers = Providers::new_with_programs(&[], &[], &[&document], &trust())?;
        let target = Target {
            loader: "fabric".to_owned(),
            provides: Vec::new(),
            loader_version: None,
            game_version: "1.0".to_owned(),
            side: Side::Client,
        };
        let hits = providers.search("catalog", &SearchHttp, "test", &target, 5)?;
        assert_eq!(hits.first().map(|hit| hit.title.as_str()), Some("Sodium"));
        Ok(())
    }

    #[test]
    fn codecs_are_validated_looked_up_and_detected() -> Result<(), RegistryError> {
        let providers = Providers::new(&[registration(&[ALPHA])], &[])?;
        assert_eq!(
            providers
                .pack_codecs()
                .first()
                .map(|codec| codec.id.as_str()),
            Some("alpha")
        );
        assert_eq!(providers.pack_codec("alpha")?.descriptor().id, "alpha");
        assert_eq!(
            providers
                .detect_pack_codec(&EmptyInput)?
                .map(|codec| codec.descriptor().id.as_str()),
            Some("alpha")
        );
        assert!(matches!(
            providers.pack_codec("missing"),
            Err(RegistryError::UnknownCodec(id)) if id == "missing"
        ));
        Ok(())
    }

    #[test]
    fn codec_registration_rejects_mismatches_and_duplicate_hints() {
        assert!(matches!(
            Providers::new(&[registration(&[MISMATCH])], &[]),
            Err(RegistryError::MismatchedCodec { .. })
        ));
        assert!(matches!(
            Providers::new(&[registration(&[ALPHA, DUPLICATE_HINT])], &[]),
            Err(RegistryError::DuplicateCodecHint { .. })
        ));
        assert!(matches!(
            Providers::new(&[registration(&[ALPHA, ALPHA])], &[]),
            Err(RegistryError::DuplicateCodec(id)) if id == "alpha"
        ));
    }

    #[test]
    fn codec_policy_and_ambiguous_detection_fail_closed() {
        let manifest = EXAMPLE.replace("requires_auth = false", "requires_auth = true");
        let mut blocked = registration(&[ALPHA]);
        blocked.manifest = Box::leak(manifest.into_boxed_str());
        let providers = Providers::new(&[blocked], &[]).unwrap();
        assert!(matches!(
            providers.pack_codec("alpha"),
            Err(RegistryError::AuthenticationRequired(id)) if id == "example"
        ));

        let providers = Providers::new(&[registration(&[ALPHA, BETA])], &[]).unwrap();
        assert!(matches!(
            providers.detect_pack_codec(&EmptyInput),
            Err(RegistryError::AmbiguousCodec(ids)) if ids == ["alpha", "beta"]
        ));
    }

    #[test]
    fn plain_http_reaches_the_direct_adapter_security_error() {
        let providers = Providers::builtins().unwrap();
        match providers.request("http://example.test/mod.jar") {
            Err(RegistryError::Adapter(AdapterError::Specific(error))) => {
                assert!(error.to_string().contains("insecure URL"), "{error}");
            }
            other => panic!("expected the direct adapter's refusal, got {other:?}"),
        }
    }

    #[test]
    fn search_returns_provider_neutral_records() -> Result<(), RegistryError> {
        let results = Providers::builtins()?.search(
            "modrinth",
            &SearchHttp,
            "rendering",
            &Target {
                loader: "fabric".to_owned(),
                provides: Vec::new(),
                loader_version: None,
                game_version: "1.21.1".to_owned(),
                side: Side::Client,
            },
            10,
        )?;
        let [result] = results.as_slice() else {
            panic!("expected one result, got {results:?}");
        };
        assert_eq!(result.provider, "modrinth");
        assert_eq!(result.project, "AANobbMI");
        assert_eq!(result.reference, "sodium");
        Ok(())
    }
}
