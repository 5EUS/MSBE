//! The fail-closed mapping from provider manifests to reviewed adapters.

use std::collections::BTreeMap;

use msbe_provider_api::{
    Adapter, AdapterError, Catalog, HttpClient, ManifestError, Overlay, OverlayError, Provider,
    Registration, Target,
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
    overlay: Overlay,
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
            overlay.extend_from_slice(registration.overlay);
        }
        Ok(Self {
            catalog,
            adapters,
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
        AdapterError, HttpClient, HttpError, PackageId, Target, model::Request,
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
