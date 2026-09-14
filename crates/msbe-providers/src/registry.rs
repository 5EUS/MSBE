//! The fail-closed mapping from provider manifests to reviewed adapters.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use msbe_core::{config::Home, instance::ExtensionPin};
use msbe_fsops::Digest;
use msbe_provider_api::manifest::Acquisition;
use msbe_provider_api::{
    Adapter, AdapterError, Catalog, ExtensionEnvelope, Handoff, HeaderError, HttpClient,
    ManifestError, Overlay, OverlayError, PackCodec, PackCodecDescriptor, PackCodecError,
    PackCodecRegistration, PackInput, ProgramError, ProgramRegistration, Provider, ProviderProgram,
    ProviderProgramEnvelope, Rate, Registration, Target, VerifyingKey, WasmPackCodecRegistration,
    model::{Account, Request, SearchResult},
    resolve::{Adapters, ResolveError},
};
use msbe_secrets::{Access, Credentials, Secret, StoreError};
use msbe_wasm_codec::WasmPackCodec;
use thiserror::Error;

use crate::{
    CredentialSource, ExtensionKind, ExtensionTrust, InstalledExtension,
    authorized::{Authorized, Keys},
    builtin::{BUILTIN, BUILTIN_PROGRAMS},
    installed::{self, Found},
    runtime,
};

const NATIVE_HOST_API_VERSION: u32 = 1;
/// The signer every extension compiled into or embedded in this build is pinned with.
const BUILD_SIGNER: &str = "msbe-build";

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
    /// Each provider's adapter, whose requests carry only what its provider's scope allows.
    adapters: BTreeMap<String, Authorized>,
    /// The credentials those requests carry, and the quotas their responses report.
    keys: Arc<Keys>,
    codecs: CodecTable,
    extensions: Vec<ExtensionPin>,
    overlay: Overlay,
    /// Each program provider's canonical program digest, which an acknowledgement is bound to.
    programs: BTreeMap<String, String>,
    /// Which providers have a credential, and which terms were acknowledged.
    access: Access,
    /// The pin of each signed program, naming its real signer, by provider id.
    signed_pins: BTreeMap<String, ExtensionPin>,
    /// Every extension found in the data directory, and whether it runs.
    installed: Vec<InstalledExtension>,
}

#[derive(Debug)]
struct RegisteredCodec {
    provider: Option<String>,
    codec: Box<dyn PackCodec>,
}

impl Providers {
    /// Adds a trusted WebAssembly pack codec to this registry, returning its codec ID.
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
    ) -> Result<String, RegistryError> {
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
    /// detection hints are free, and returns its ID.
    fn admit_wasm_codec(&mut self, codec: WasmPackCodec) -> Result<String, RegistryError> {
        let descriptor = codec.descriptor();
        self.codecs.claim(descriptor)?;
        let (id, provider) = (descriptor.id.clone(), descriptor.provider.clone());
        self.codecs.codecs.insert(
            id.clone(),
            RegisteredCodec {
                provider,
                codec: Box::new(codec),
            },
        );
        Ok(id)
    }

    /// Adds an installed codec, once neither its signer nor its digest is revoked.
    fn install_codec(
        &mut self,
        envelope: &ExtensionEnvelope<Vec<u8>>,
        trust: &ExtensionTrust,
    ) -> Result<String, RegistryError> {
        if trust.is_revoked_signer(&envelope.signer) {
            return Err(RegistryError::RevokedCodecSigner(envelope.signer.clone()));
        }
        if trust.is_revoked_digest(&envelope.package_digest) {
            return Err(RegistryError::RevokedCodec(envelope.package_digest.clone()));
        }
        self.register_wasm_codec(envelope, trust)
    }

    /// The providers MSBE ships, plus the provider programs and WebAssembly pack codecs installed
    /// in `home` that its `extensions/trust.toml` trusts (`docs/18-wasm-extensions.md` §18.3),
    /// authorized by the credentials and acknowledgements `home` records
    /// (`docs/07-browser-and-secrets.md` §7.5). Requests that may carry a credential read it from
    /// `home`'s credentials when first needed.
    ///
    /// Each installed extension is admitted or refused on its own. One that cannot be read,
    /// verified, trusted or registered is skipped, and [`Providers::installed_extensions`] says
    /// why; it never stops another, or a provider MSBE ships. A trust root that cannot be read
    /// trusts nothing. Programs are admitted before codecs, so a codec can bind to an installed
    /// provider.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Access`] when the credential records or acknowledgements cannot be
    /// read, and another [`RegistryError`] only when a provider MSBE ships is invalid.
    pub fn installed(home: &Home) -> Result<Self, RegistryError> {
        let found = installed::read(&home.root().join(installed::DIRECTORY));
        let mut listed = Vec::new();
        let trust = found.trust.unwrap_or_else(|error| {
            listed.push(InstalledExtension::unreadable(
                ExtensionKind::TrustRoot,
                installed::trust_file(home),
                &error,
            ));
            ExtensionTrust::default()
        });
        let mut documents = builtin_manifests()?;
        let programs = admit_programs(found.programs, &trust, &mut documents, &mut listed);
        let mut providers = Self::assemble(BUILTIN, BUILTIN_PROGRAMS, &[], programs)?
            .with_access(Access::load(home)?)
            .with_credentials(Arc::new(Mutex::new(Credentials::open(home)?)));
        for Found { path, item } in found.codecs {
            listed.push(match item {
                Ok(codec) => {
                    let refusal = providers.install_codec(&codec.envelope, &trust).err();
                    InstalledExtension::from_envelope(
                        ExtensionKind::Codec,
                        path,
                        &codec.envelope,
                        refusal.as_ref(),
                    )
                }
                Err(error) => InstalledExtension::unreadable(ExtensionKind::Codec, path, &error),
            });
        }
        providers.installed = listed;
        Ok(providers)
    }

    /// Every extension found in the data directory by [`Providers::installed`], in the order
    /// found, and whether it runs. A trust root that cannot be read is listed first.
    pub fn installed_extensions(&self) -> &[InstalledExtension] {
        &self.installed
    }

    /// The providers MSBE ships: its native exceptions, and the programs it ships.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] if a built-in manifest, program or overlay entry is invalid.
    /// This indicates a build error in MSBE rather than user-provided input.
    pub fn builtins() -> Result<Self, RegistryError> {
        Self::assemble(BUILTIN, BUILTIN_PROGRAMS, &[], Vec::new())
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
        Self::assemble(registrations, &[], manifests, Vec::new())
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
        Self::assemble(
            registrations,
            &[],
            manifests,
            verified_programs(program_documents, trust)?,
        )
    }

    /// Registers native `registrations`, the programs this build ships, `manifests` nothing
    /// serves, and `signed` programs a trust root has already accepted.
    fn assemble(
        registrations: &[Registration],
        shipped: &[ProgramRegistration],
        manifests: &[&str],
        signed: Vec<SignedProgram>,
    ) -> Result<Self, RegistryError> {
        let shipped = shipped
            .iter()
            .map(|registration| Ok((registration, shipped_program(registration)?)))
            .collect::<Result<Vec<_>, RegistryError>>()?;
        let program_manifests: Vec<String> = shipped
            .iter()
            .map(|(_, program)| program)
            .chain(signed.iter().map(|signed| &signed.program))
            .map(provider_manifest)
            .collect::<Result<_, _>>()?;
        let documents: Vec<&str> = registrations
            .iter()
            .map(|registration| registration.manifest)
            .chain(manifests.iter().copied())
            .chain(program_manifests.iter().map(String::as_str))
            .collect();
        let catalog = Catalog::from_toml(&documents)?;
        let mut assembly = Assembly::default();
        for registration in registrations {
            assembly.add_native(&catalog, registration)?;
        }
        for (registration, program) in &shipped {
            assembly
                .extensions
                .push(program_pin(registration, program)?);
            assembly.overlay.extend_from_slice(registration.overlay);
        }
        let signed_pins: BTreeMap<String, ExtensionPin> = signed
            .iter()
            .map(|signed| (signed.program.provider.id.clone(), signed.pin.clone()))
            .collect();
        let mut programs = BTreeMap::new();
        for program in shipped
            .iter()
            .map(|(_, program)| program.clone())
            .chain(signed.into_iter().map(|signed| signed.program))
        {
            programs.insert(
                program.provider.id.clone(),
                ProviderProgramEnvelope::digest_for(&program)?,
            );
            assembly.add_program(&catalog, program)?;
        }
        let mut providers = Self {
            catalog,
            adapters: assembly.adapters,
            keys: assembly.keys,
            codecs: assembly.codecs,
            extensions: assembly.extensions,
            overlay: Overlay::from_toml(&assembly.overlay)?,
            programs,
            access: Access::none(),
            signed_pins,
            installed: Vec::new(),
        };
        let modules = registrations
            .iter()
            .map(|registration| -> (&str, &[WasmPackCodecRegistration]) {
                (registration.id, registration.wasm_pack_codecs)
            })
            .chain(shipped.iter().map(
                |(registration, program)| -> (&str, &[WasmPackCodecRegistration]) {
                    (&program.provider.id, registration.wasm_pack_codecs)
                },
            ));
        for (provider, codecs) in modules {
            for codec in codecs {
                providers.register_shipped_codec(provider, codec)?;
            }
        }
        Ok(providers)
    }

    /// This registry, admitting a provider that needs a credential or acknowledged terms once
    /// `access` records them. Until then, every such provider is refused.
    #[must_use]
    pub fn with_access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }

    /// This registry, with each provider's credential read from `source` when a request that may
    /// carry it is made: one to the origin of the provider's `api_base`, from an adapter that names
    /// a credential header. Without a source, no request carries a credential.
    #[must_use]
    pub fn with_credentials(self, source: Arc<dyn CredentialSource>) -> Self {
        self.keys.set_source(source);
        self
    }

    /// The quota `provider`'s API last reported remaining to this registry, if it reported any.
    pub fn rate(&self, provider: &str) -> Option<Rate> {
        self.keys.rate(provider)
    }

    /// The account `candidate` belongs to at `provider`, checked before the credential is kept.
    /// The provider's terms must be acknowledged; no stored credential is needed.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the provider is unknown, its terms are not acknowledged, it
    /// cannot check a credential, or the check fails, as when the provider refuses the credential.
    pub fn validate_credential(
        &self,
        provider: &str,
        http: &dyn HttpClient,
        candidate: &Secret,
    ) -> Result<Account, RegistryError> {
        self.acknowledged(self.catalog.provider(provider)?)?;
        let adapter = self
            .adapters
            .get(provider)
            .ok_or_else(|| RegistryError::UnavailableAdapter(provider.to_owned()))?;
        if adapter.as_accounts().is_none() {
            return Err(RegistryError::AccountsUnavailable(provider.to_owned()));
        }
        Ok(adapter.account_with(http, candidate)?)
    }

    /// The handoff capability of the provider whose links use `uri`'s scheme, once its policy has
    /// been checked. Only the scheme is read; the link, which carries a key, is never repeated.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when no provider claims the scheme, or it is prohibited by policy.
    pub fn handoff(&self, uri: &str) -> Result<&dyn Handoff, RegistryError> {
        let scheme = uri
            .split_once("://")
            .map(|(scheme, _)| scheme.to_ascii_lowercase())
            .unwrap_or_default();
        let provider = self
            .catalog
            .handoff_provider(&scheme)
            .ok_or_else(|| RegistryError::UnknownHandoffScheme(scheme.clone()))?;
        self.permitted(provider)?
            .as_handoff()
            .ok_or(RegistryError::UnknownHandoffScheme(scheme))
    }

    /// The scheme of each enabled provider's handoff links, with the id of the provider that claims
    /// it: the map [`Self::handoff`] routes links by, before any policy check.
    pub fn handoff_schemes(&self) -> BTreeMap<String, String> {
        self.catalog
            .handoff_schemes()
            .map(|(scheme, provider)| (scheme.to_owned(), provider.id.clone()))
            .collect()
    }

    /// Every enabled provider that fetches content with a tool the user registers, ordered by id.
    pub fn tool_providers(&self) -> Vec<&Provider> {
        self.catalog
            .providers()
            .filter(|provider| provider.acquisition == Acquisition::ExternalTool {})
            .collect()
    }

    /// The digest of the program serving `provider`, when a program serves it: what an
    /// acknowledgement of its terms is recorded against.
    pub fn program_digest(&self, provider: &str) -> Option<&str> {
        self.programs.get(provider).map(String::as_str)
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
                        .is_ok_and(|provider| self.authorize(provider).is_ok())
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

    /// The pins an export of content from `providers` records: every extension reviewed into this
    /// build, and the signed program of each of `providers` served by one, with its real signer.
    /// A signed program the content does not use is not pinned, so installing one never stops a
    /// pack from importing elsewhere.
    #[must_use]
    pub fn extension_pins_for<'a>(
        &self,
        providers: impl IntoIterator<Item = &'a str>,
    ) -> Vec<ExtensionPin> {
        let mut pins = self.extension_pins();
        pins.extend(
            providers
                .into_iter()
                .filter_map(|provider| self.signed_pins.get(provider).cloned()),
        );
        pins
    }

    /// Checks that this registry serves what `pins` name: precisely the extensions reviewed into
    /// this build, and, for each signed program pinned, a trusted program of that id and digest.
    ///
    /// Empty pins are accepted for native bundles produced before extension pinning was added.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::ExtensionPinsDiffer`] when the build's extensions differ, and
    /// [`RegistryError::MissingExtension`] naming the first signed program that is not installed
    /// and trusted here, or is installed with other content.
    pub fn check_extension_pins(&self, pins: &[ExtensionPin]) -> Result<(), RegistryError> {
        if pins.is_empty() {
            return Ok(());
        }
        let (build, signed): (Vec<&ExtensionPin>, Vec<&ExtensionPin>) =
            pins.iter().partition(|pin| pin.signer == BUILD_SIGNER);
        let mut expected = self.extension_pins();
        expected.sort_by(|left, right| left.id.cmp(&right.id));
        if !build.into_iter().eq(expected.iter()) {
            return Err(RegistryError::ExtensionPinsDiffer);
        }
        match signed.into_iter().find(|pin| {
            self.signed_pins
                .get(&pin.id)
                .is_none_or(|installed| installed.digest != pin.digest)
        }) {
            Some(missing) => Err(RegistryError::MissingExtension {
                id: missing.id.clone(),
                version: missing.version.clone(),
                signer: missing.signer.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Whether [`Providers::check_extension_pins`] accepts `pins`.
    #[must_use]
    pub fn accepts_extension_pins(&self, pins: &[ExtensionPin]) -> bool {
        self.check_extension_pins(pins).is_ok()
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
            self.authorize(self.catalog.provider(provider_id)?)?;
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
        self.authorize(provider)?;
        self.adapters
            .get(&provider.id)
            .map(|adapter| -> &dyn Adapter { adapter })
            .ok_or_else(|| RegistryError::UnavailableAdapter(provider.id.clone()))
    }

    /// Refuses a provider whose terms must be acknowledged, or that needs a credential, until its
    /// [`Access`] records them. Terms come first: signing in to a service means accepting them.
    fn authorize(&self, provider: &Provider) -> Result<(), RegistryError> {
        self.acknowledged(provider)?;
        if provider.policy.requires_auth && !self.access.is_authenticated(&provider.id) {
            return Err(RegistryError::AuthenticationRequired(provider.id.clone()));
        }
        Ok(())
    }

    /// Refuses a provider whose current terms, under its current program, must be acknowledged
    /// and are not.
    fn acknowledged(&self, provider: &Provider) -> Result<(), RegistryError> {
        let policy = &provider.policy;
        let program = self.programs.get(&provider.id).map(String::as_str);
        if policy.ack_required
            && !self
                .access
                .has_acknowledged(&provider.id, &policy.tos_url, program)
        {
            return Err(RegistryError::AcknowledgementRequired {
                provider: provider.id.clone(),
                terms: policy.tos_url.clone(),
            });
        }
        Ok(())
    }
}

/// A signed program a trust root accepted, and the pin an export records for it.
struct SignedProgram {
    program: ProviderProgram,
    pin: ExtensionPin,
}

/// The signed programs in `documents` that `trust` accepts, refusing any it does not.
fn verified_programs(
    documents: &[&str],
    trust: &ProgramTrust,
) -> Result<Vec<SignedProgram>, RegistryError> {
    documents
        .iter()
        .map(|document| {
            let envelope = ProviderProgramEnvelope::from_toml(document)?;
            verify_program(&envelope, trust)?;
            Ok(SignedProgram {
                pin: signed_pin(&envelope)?,
                program: envelope.0.payload,
            })
        })
        .collect()
}

/// Refuses `envelope` when its signer or digest is revoked, or its signature does not verify
/// against a key `trust` holds for its signer.
fn verify_program(
    envelope: &ProviderProgramEnvelope,
    trust: &ProgramTrust,
) -> Result<(), RegistryError> {
    let signed = &envelope.0;
    if trust.revoked_signers.contains(&signed.signer) {
        return Err(RegistryError::RevokedProgramSigner(signed.signer.clone()));
    }
    if trust.revoked_digests.contains(&signed.package_digest)
        || trust
            .revoked_digests
            .contains(&signed.package_digest.to_ascii_lowercase())
    {
        return Err(RegistryError::RevokedProgram(signed.package_digest.clone()));
    }
    Ok(envelope.verify(&trust.trusted_keys)?)
}

/// The installed program envelopes `trust` admits, one at a time, with each found recorded in
/// `listed`, admitted or refused.
///
/// A program is admitted when it parses and validates, neither its signer nor its digest is
/// revoked, its signature verifies against a trusted key, its signer is granted its provider id,
/// it is installed under that id, and its provider collides with none in `documents`: the providers
/// MSBE ships and the programs admitted before it. Each admitted provider joins `documents`.
fn admit_programs(
    found: Vec<Found<String>>,
    trust: &ExtensionTrust,
    documents: &mut Vec<String>,
    listed: &mut Vec<InstalledExtension>,
) -> Vec<SignedProgram> {
    let program_trust = trust.program_trust();
    let mut admitted = Vec::new();
    for Found { path, item } in found {
        let envelope = match item.and_then(|document| {
            ProviderProgramEnvelope::from_toml(&document).map_err(RegistryError::from)
        }) {
            Ok(envelope) => envelope,
            Err(error) => {
                listed.push(InstalledExtension::unreadable(
                    ExtensionKind::Program,
                    path,
                    &error,
                ));
                continue;
            }
        };
        match admit_program(&path, &envelope, trust, &program_trust, documents) {
            Ok((manifest, pin)) => {
                listed.push(InstalledExtension::from_envelope(
                    ExtensionKind::Program,
                    path,
                    &envelope.0,
                    None,
                ));
                documents.push(manifest);
                admitted.push(SignedProgram {
                    pin,
                    program: envelope.0.payload,
                });
            }
            Err(error) => listed.push(InstalledExtension::from_envelope(
                ExtensionKind::Program,
                path,
                &envelope.0,
                Some(&error),
            )),
        }
    }
    admitted
}

/// Checks the installed program `envelope`, found at `path`, against `trust` and the provider
/// manifests in `documents`, returning its own manifest and pin.
fn admit_program(
    path: &Path,
    envelope: &ProviderProgramEnvelope,
    trust: &ExtensionTrust,
    program_trust: &ProgramTrust,
    documents: &[String],
) -> Result<(String, ExtensionPin), RegistryError> {
    verify_program(envelope, program_trust)?;
    let provider = &envelope.0.payload.provider.id;
    let signer = &envelope.0.signer;
    if !trust.may_introduce(signer, provider) {
        return Err(RegistryError::ProgramNotGranted {
            provider: provider.clone(),
            signer: signer.clone(),
        });
    }
    let file = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if envelope.0.id != *provider || file != format!("{provider}.toml") {
        return Err(RegistryError::MisnamedProgram {
            provider: provider.clone(),
            id: envelope.0.id.clone(),
            file,
        });
    }
    let manifest = provider_manifest(&envelope.0.payload)?;
    let mut candidate: Vec<&str> = documents.iter().map(String::as_str).collect();
    candidate.push(&manifest);
    Catalog::from_toml(&candidate)?;
    Ok((manifest, signed_pin(envelope)?))
}

/// Checks the provider program envelope at `path` as installing it into the extensions directory
/// `directory` would: against that directory's trust root, the providers MSBE ships, and the
/// programs installed there under other files. Nothing is installed.
///
/// # Errors
///
/// Returns [`RegistryError`] naming why the program would be refused.
pub(crate) fn check_installable_program(
    directory: &Path,
    path: &Path,
) -> Result<ProviderProgramEnvelope, RegistryError> {
    let found = installed::read(directory);
    let trust = found.trust?;
    let others = found
        .programs
        .into_iter()
        .filter(|other| !same_file(&other.path, path))
        .collect();
    let mut documents = builtin_manifests()?;
    admit_programs(others, &trust, &mut documents, &mut Vec::new());
    let envelope = ProviderProgramEnvelope::from_toml(&installed::read_program(path)?)?;
    admit_program(path, &envelope, &trust, &trust.program_trust(), &documents)?;
    Ok(envelope)
}

/// Whether `left` and `right` name the same file.
fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// The provider manifests of everything this build ships, which no installed program may collide
/// with.
fn builtin_manifests() -> Result<Vec<String>, RegistryError> {
    BUILTIN
        .iter()
        .map(|registration| Ok(registration.manifest.to_owned()))
        .chain(
            BUILTIN_PROGRAMS
                .iter()
                .map(|registration| provider_manifest(&shipped_program(registration)?)),
        )
        .collect()
}

/// `program`'s provider, as a manifest document.
fn provider_manifest(program: &ProviderProgram) -> Result<String, RegistryError> {
    toml::to_string(&program.provider)
        .map_err(|error| RegistryError::Program(ProgramError::Canonical(error.to_string())))
}

/// The canonical digest of `program`, which pins and acknowledgements name.
fn program_digest(program: &ProviderProgram) -> Result<Digest, RegistryError> {
    format!("sha256:{}", ProviderProgramEnvelope::digest_for(program)?)
        .parse()
        .map_err(|error: msbe_fsops::Error| RegistryError::NativeCanonical(error.to_string()))
}

/// The pin of a signed program: its provider and digest, and the version, host API and signer its
/// envelope names.
fn signed_pin(envelope: &ProviderProgramEnvelope) -> Result<ExtensionPin, RegistryError> {
    let envelope = &envelope.0;
    Ok(ExtensionPin {
        id: envelope.payload.provider.id.clone(),
        version: envelope.version.clone(),
        digest: program_digest(&envelope.payload)?,
        host_api_minimum: envelope.host_api.minimum,
        host_api_maximum: envelope.host_api.maximum,
        signer: envelope.signer.clone(),
    })
}

/// The program `registration` ships, validated.
fn shipped_program(registration: &ProgramRegistration) -> Result<ProviderProgram, RegistryError> {
    let program: ProviderProgram = toml::from_str(registration.program)
        .map_err(|error| ProgramError::Parse(error.to_string()))?;
    program.validate()?;
    Ok(program)
}

/// The pin of a program this build ships: its provider, version, and canonical digest.
fn program_pin(
    registration: &ProgramRegistration,
    program: &ProviderProgram,
) -> Result<ExtensionPin, RegistryError> {
    Ok(ExtensionPin {
        id: program.provider.id.clone(),
        version: registration.version.to_owned(),
        digest: program_digest(program)?,
        host_api_minimum: NATIVE_HOST_API_VERSION,
        host_api_maximum: NATIVE_HOST_API_VERSION,
        signer: BUILD_SIGNER.to_owned(),
    })
}

/// A registry being assembled: the adapters, codecs, pins and overlay entries gathered so far.
#[derive(Default)]
struct Assembly {
    adapters: BTreeMap<String, Authorized>,
    keys: Arc<Keys>,
    codecs: CodecTable,
    extensions: Vec<ExtensionPin>,
    overlay: Vec<&'static str>,
}

impl Assembly {
    /// Adds a native registration: its adapter, pin, codecs and overlay entries.
    fn add_native(
        &mut self,
        catalog: &Catalog,
        registration: &Registration,
    ) -> Result<(), RegistryError> {
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
        self.extensions
            .push(native_pin(&registration.identity, manifest)?);
        let provider = catalog.provider(registration.id)?;
        let adapter = (registration.build)(provider)?;
        if adapter.id() != provider.id {
            return Err(RegistryError::MismatchedAdapter {
                manifest: provider.id.clone(),
                adapter: adapter.id().to_owned(),
            });
        }
        self.adapters.insert(
            provider.id.clone(),
            Authorized::new(provider, adapter, Arc::clone(&self.keys))?,
        );
        for codec_registration in registration.pack_codecs {
            let pin = self.codecs.register(&provider.id, codec_registration)?;
            self.extensions.push(pin);
        }
        self.overlay.extend_from_slice(registration.overlay);
        Ok(())
    }

    /// Adds the adapter a program's reviewed runtime builds.
    fn add_program(
        &mut self,
        catalog: &Catalog,
        program: ProviderProgram,
    ) -> Result<(), RegistryError> {
        let provider = catalog.provider(&program.provider.id)?;
        let adapter = runtime::build(program);
        if adapter.id() != provider.id {
            return Err(RegistryError::MismatchedAdapter {
                manifest: provider.id.clone(),
                adapter: adapter.id().to_owned(),
            });
        }
        let adapter = Authorized::new(provider, adapter, Arc::clone(&self.keys))?;
        if self.adapters.insert(provider.id.clone(), adapter).is_some() {
            return Err(RegistryError::DuplicateProgramAdapter(provider.id.clone()));
        }
        Ok(())
    }
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
    /// An installed codec's signer is revoked.
    #[error("codec signer {0:?} is revoked")]
    RevokedCodecSigner(String),
    /// An installed codec's digest is revoked.
    #[error("codec digest {0:?} is revoked")]
    RevokedCodec(String),
    /// An installed program introduces a provider its signer is not granted.
    #[error(
        "provider program {provider:?} is signed by {signer:?}, whose trust entry does not grant programs = [\"{provider}\"]"
    )]
    ProgramNotGranted {
        /// The provider id the program introduces.
        provider: String,
        /// The envelope's signer.
        signer: String,
    },
    /// An installed program is not installed under its provider id.
    #[error(
        "provider program {provider:?} must be installed as {provider}.toml with extension id {provider:?}, not as {file} with id {id:?}"
    )]
    MisnamedProgram {
        /// The provider id the program introduces.
        provider: String,
        /// The envelope's extension id.
        id: String,
        /// The envelope document's file name.
        file: String,
    },
    /// Content pins a signed program this installation does not serve.
    #[error(
        "requires provider program {id} {version} signed by {signer}, which is not installed and trusted here, or is installed with different content"
    )]
    MissingExtension {
        /// The provider id.
        id: String,
        /// The pinned version.
        version: String,
        /// The pinned signer.
        signer: String,
    },
    /// Content pins other extension code than this build reviewed.
    #[error("the native bundle requires different reviewed extension code")]
    ExtensionPinsDiffer,
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
    /// An adapter declares a header its provider's requests cannot use.
    #[error("provider {provider:?} declares an unusable API header: {source}")]
    ApiHeaders {
        /// The provider id.
        provider: String,
        /// What is wrong with the header.
        #[source]
        source: HeaderError,
    },
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
    /// The provider has no way to check which account a credential belongs to.
    #[error("provider {0:?} cannot check a credential")]
    AccountsUnavailable(String),
    /// No permitted provider handles links with this scheme.
    #[error("no provider handles {0}:// links")]
    UnknownHandoffScheme(String),
    /// The adapter does not support search.
    #[error("provider {0:?} does not support search")]
    SearchUnavailable(String),
    /// The provider needs a credential, and none is stored or set in the environment.
    #[error("provider {0:?} requires signing in, and no credential is stored for it")]
    AuthenticationRequired(String),
    /// The provider's current terms, under its current program, have not been acknowledged.
    #[error("provider {provider:?} requires acknowledging its terms {terms:?} before it is used")]
    AcknowledgementRequired {
        /// The provider id.
        provider: String,
        /// The terms the user would need to acknowledge.
        terms: String,
    },
    /// The provider's adapter rejected a reference or failed a request.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// The data directory's credential records or acknowledgements cannot be read.
    #[error(transparent)]
    Access(#[from] StoreError),
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
