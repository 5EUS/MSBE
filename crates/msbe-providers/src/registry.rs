//! The fail-closed mapping from provider manifests to reviewed adapters.

use std::collections::BTreeMap;

use msbe_provider_api::{
    Adapter, AdapterError, Catalog, HttpClient, ManifestError, Overlay, OverlayError, PackCodec,
    PackCodecDescriptor, PackCodecError, PackInput, Provider, Registration, Target,
    model::{Request, SearchResult},
    resolve::{Adapters, ResolveError},
};
use thiserror::Error;

/// The adapters MSBE ships. A new provider is a crate beside these and one line here.
pub const BUILTIN: &[Registration] = &[
    msbe_provider_direct::REGISTRATION,
    msbe_provider_modrinth::REGISTRATION,
];

/// A user-entered source, routed to its provider and parsed by that provider's adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    /// The provider id.
    pub provider: String,
    /// What the source asks the provider for.
    pub request: Request,
}

/// Validated provider manifests, the adapters that serve them, and the overlay those adapters
/// ship.
#[derive(Debug)]
pub struct Providers {
    catalog: Catalog,
    adapters: BTreeMap<String, Box<dyn Adapter>>,
    codecs: BTreeMap<String, RegisteredCodec>,
    overlay: Overlay,
}

#[derive(Debug)]
struct RegisteredCodec {
    provider: String,
    codec: Box<dyn PackCodec>,
}

impl Providers {
    /// The providers MSBE ships.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] if a built-in manifest or overlay entry is invalid. This
    /// indicates a build error in MSBE rather than user-provided input.
    pub fn builtins() -> Result<Self, RegistryError> {
        Self::new(BUILTIN, &[])
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
        let documents: Vec<&str> = registrations
            .iter()
            .map(|registration| registration.manifest)
            .chain(manifests.iter().copied())
            .collect();
        let catalog = Catalog::from_toml(&documents)?;
        let mut adapters = BTreeMap::new();
        let mut codecs = BTreeMap::new();
        let mut extensions = BTreeMap::new();
        let mut media_types = BTreeMap::new();
        let mut overlay = Vec::new();
        for registration in registrations {
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
                let codec = (codec_registration.build)()?;
                let descriptor = codec.descriptor();
                descriptor.validate()?;
                if descriptor.id != codec_registration.id {
                    return Err(RegistryError::MismatchedCodec {
                        registration: codec_registration.id.to_owned(),
                        descriptor: descriptor.id.clone(),
                    });
                }
                if descriptor
                    .provider
                    .as_deref()
                    .is_some_and(|id| id != provider.id)
                {
                    return Err(RegistryError::MismatchedCodecProvider {
                        codec: descriptor.id.clone(),
                        registration: provider.id.clone(),
                        descriptor: descriptor.provider.clone(),
                    });
                }
                if codecs.contains_key(&descriptor.id) {
                    return Err(RegistryError::DuplicateCodec(descriptor.id.clone()));
                }
                for extension in &descriptor.extensions {
                    register_hint(&mut extensions, extension, &descriptor.id)?;
                }
                for media_type in &descriptor.media_types {
                    register_hint(&mut media_types, media_type, &descriptor.id)?;
                }
                codecs.insert(
                    descriptor.id.clone(),
                    RegisteredCodec {
                        provider: provider.id.clone(),
                        codec,
                    },
                );
            }
            overlay.extend_from_slice(registration.overlay);
        }
        Ok(Self {
            catalog,
            adapters,
            codecs,
            overlay: Overlay::from_toml(&overlay)?,
        })
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
            .values()
            .filter(|registered| {
                self.catalog
                    .provider(&registered.provider)
                    .is_ok_and(|provider| authorize(provider).is_ok())
            })
            .map(|registered| registered.codec.descriptor())
            .collect()
    }

    /// Looks up a reviewed pack codec after enforcing its provider's policy.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the codec is unknown or its provider is prohibited.
    pub fn pack_codec(&self, id: &str) -> Result<&dyn PackCodec, RegistryError> {
        let registered = self
            .codecs
            .get(id)
            .ok_or_else(|| RegistryError::UnknownCodec(id.to_owned()))?;
        authorize(self.catalog.provider(&registered.provider)?)?;
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

fn register_hint(
    hints: &mut BTreeMap<String, String>,
    hint: &str,
    codec: &str,
) -> Result<(), RegistryError> {
    let normalized = hint.to_ascii_lowercase();
    if let Some(existing) = hints.insert(normalized.clone(), codec.to_owned()) {
        return Err(RegistryError::DuplicateCodecHint {
            hint: normalized,
            first: existing,
            second: codec.to_owned(),
        });
    }
    Ok(())
}

/// Why a provider could not be used through the reviewed adapter registry.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegistryError {
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
    use std::io::Write;

    use msbe_plan_schema::Side;
    use msbe_provider_api::{
        Adapter, AdapterError, ContainerKind, HttpClient, HttpError, PackCodec,
        PackCodecDescriptor, PackCodecError, PackCodecRegistration, PackDirections, PackEntry,
        PackExportContext, PackExportPlan, PackImportContext, PackImportPlan, PackInput,
        PackLayout, PackOptionSchema, PackOptions, PackProbe, PackageId, Provider, Registration,
        SupportSet, Target,
        model::Request,
    };
    use msbe_provider_direct::DirectError;
    use serde_json::json;

    use super::{Providers, RegistryError, Routed};

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
        fn container(&self) -> ContainerKind { ContainerKind::File }
        fn entries(&self) -> &[PackEntry] { &[] }
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
        build: build_alpha,
    };
    const BETA: PackCodecRegistration = PackCodecRegistration {
        id: "beta",
        build: build_beta,
    };
    const DUPLICATE_HINT: PackCodecRegistration = PackCodecRegistration {
        id: "beta",
        build: build_duplicate_hint,
    };
    const MISMATCH: PackCodecRegistration = PackCodecRegistration {
        id: "registered",
        build: build_mismatch,
    };

    fn registration(codecs: &'static [PackCodecRegistration]) -> Registration {
        Registration {
            id: "example",
            manifest: EXAMPLE,
            overlay: &[],
            build: build_example,
            pack_codecs: codecs,
        }
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
            Err(RegistryError::Adapter(AdapterError::Specific(error))) => assert!(matches!(
                error.downcast_ref::<DirectError>(),
                Some(DirectError::Insecure(_))
            )),
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
