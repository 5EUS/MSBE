//! The reviewed runtime adapters available for validated provider manifests.

use thiserror::Error;

use crate::{
    Catalog, HttpClient, ManifestError, Provider, SearchResult, Target,
    direct::DirectSource,
    modrinth::{Modrinth, Spec},
};

/// The stable identifier of the built-in direct URL provider.
pub const DIRECT: &str = "url";
/// The stable identifier of the built-in Modrinth provider.
pub const MODRINTH: &str = "modrinth";

/// A fail-closed mapping from provider manifests to reviewed runtime adapters.
#[derive(Debug, Clone, Copy)]
pub struct ProviderRegistry<'a> {
    catalog: &'a Catalog,
}

impl<'a> ProviderRegistry<'a> {
    /// Creates a registry over a validated provider catalog.
    pub const fn new(catalog: &'a Catalog) -> Self {
        Self { catalog }
    }

    /// Parses a user-entered source using its reviewed provider adapter.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the source has no manifest, its provider is unavailable,
    /// its policy prohibits use, or the provider-specific reference is invalid.
    pub fn source(&self, raw: &str) -> Result<ResolvedSource<'a>, RegistryError> {
        let source = self.catalog.source(raw)?;
        let provider = source.provider();
        Self::authorize(provider)?;
        match provider.id.as_str() {
            MODRINTH => Ok(ResolvedSource::Modrinth {
                provider,
                spec: Spec::parse(source.reference())?,
            }),
            DIRECT => Ok(ResolvedSource::Direct {
                provider,
                source: DirectSource::parse(source.raw())?,
            }),
            _ => Err(RegistryError::UnavailableAdapter(provider.id.clone())),
        }
    }

    /// Opens the reviewed Modrinth adapter declared by the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] if Modrinth is absent, its manifest is incomplete, or its
    /// declared policy prohibits use.
    pub fn modrinth(&self, http: &'a dyn HttpClient) -> Result<Modrinth<'a>, RegistryError> {
        let provider = self.catalog.provider(MODRINTH)?;
        Self::authorize(provider)?;
        let api_base = provider
            .api_base()
            .ok_or_else(|| ManifestError::MissingMetadata(provider.id.clone()))?;
        Ok(Modrinth::with_base(http, api_base))
    }

    /// Searches one reviewed provider and returns provider-neutral project records.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the provider is unavailable, prohibited by policy, does
    /// not support search, or its reviewed adapter cannot complete the request.
    pub fn search(
        &self,
        provider_id: &str,
        http: &'a dyn HttpClient,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, RegistryError> {
        match provider_id {
            MODRINTH => Ok(self
                .modrinth(http)?
                .search(query, target, limit)?
                .into_iter()
                .map(|hit| SearchResult {
                    provider: MODRINTH.to_owned(),
                    project: hit.project_id,
                    reference: hit.slug,
                    title: hit.title,
                    description: hit.description,
                    downloads: hit.downloads,
                })
                .collect()),
            _ => Err(RegistryError::SearchUnavailable(provider_id.to_owned())),
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
}

/// A user source parsed by its reviewed provider adapter.
#[derive(Debug, Clone)]
pub enum ResolvedSource<'a> {
    /// A Modrinth project reference.
    Modrinth {
        /// The manifest governing this source.
        provider: &'a Provider,
        /// The parsed project reference.
        spec: Spec,
    },
    /// A direct HTTPS URL.
    Direct {
        /// The manifest governing this source.
        provider: &'a Provider,
        /// The parsed URL and optional checksum.
        source: DirectSource,
    },
}

/// Why a manifest could not be used through the reviewed adapter registry.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The source or required provider is absent from the manifest catalog.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// The manifest names a provider without a reviewed adapter in this MSBE release.
    #[error("provider {0:?} is declared but has no reviewed adapter in this MSBE release")]
    UnavailableAdapter(String),
    /// The reviewed adapter does not expose provider search.
    #[error("provider {0:?} does not support search")]
    SearchUnavailable(String),
    /// The provider requires an authentication flow which M1 does not implement.
    #[error("provider {0:?} requires authentication, which is not implemented in M1")]
    AuthenticationRequired(String),
    /// The provider requires a persisted terms acknowledgement which M1 does not implement.
    #[error(
        "provider {provider:?} requires acknowledgement of {terms:?}, which is not implemented in M1"
    )]
    AcknowledgementRequired {
        /// The provider id.
        provider: String,
        /// The terms the user would need to acknowledge.
        terms: String,
    },
    /// The reviewed Modrinth adapter rejected its reference.
    #[error(transparent)]
    Modrinth(#[from] crate::modrinth::ModrinthError),
    /// The reviewed direct URL adapter rejected its source.
    #[error(transparent)]
    Direct(#[from] crate::direct::DirectError),
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use msbe_plan_schema::Side;
    use serde_json::json;

    use super::{MODRINTH, ProviderRegistry, RegistryError, ResolvedSource};
    use crate::{Catalog, HttpClient, HttpError, Target};

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
    fn builtins_resolve_only_through_their_reviewed_adapters() -> Result<(), RegistryError> {
        let catalog = Catalog::builtins()?;
        let registry = ProviderRegistry::new(&catalog);
        assert!(matches!(
            registry.source("modrinth:sodium")?,
            ResolvedSource::Modrinth { .. }
        ));
        assert!(matches!(
            registry.source("https://example.test/mod.jar")?,
            ResolvedSource::Direct { .. }
        ));
        Ok(())
    }

    #[test]
    fn declared_provider_without_an_adapter_is_rejected() {
        let catalog = Catalog::from_toml(&[EXAMPLE]).unwrap();
        let registry = ProviderRegistry::new(&catalog);
        assert!(matches!(
            registry.source("example:mod"),
            Err(RegistryError::UnavailableAdapter(id)) if id == "example"
        ));
    }

    #[test]
    fn acknowledgement_required_providers_are_blocked_before_use() {
        let manifest = EXAMPLE.replace("ack_required = false", "ack_required = true");
        let catalog = Catalog::from_toml(&[&manifest]).unwrap();
        let registry = ProviderRegistry::new(&catalog);
        assert!(matches!(
            registry.source("example:mod"),
            Err(RegistryError::AcknowledgementRequired { .. })
        ));
    }

    #[test]
    fn authentication_required_providers_are_blocked_before_use() {
        let manifest = EXAMPLE.replace("requires_auth = false", "requires_auth = true");
        let catalog = Catalog::from_toml(&[&manifest]).unwrap();
        let registry = ProviderRegistry::new(&catalog);
        assert!(matches!(
            registry.source("example:mod"),
            Err(RegistryError::AuthenticationRequired(id)) if id == "example"
        ));
    }

    #[test]
    fn plain_http_reaches_the_direct_adapter_security_error() {
        let catalog = Catalog::builtins().unwrap();
        let registry = ProviderRegistry::new(&catalog);
        assert!(matches!(
            registry.source("http://example.test/mod.jar"),
            Err(RegistryError::Direct(crate::direct::DirectError::Insecure(
                _
            )))
        ));
    }

    #[test]
    fn search_returns_provider_neutral_records() -> Result<(), RegistryError> {
        let catalog = Catalog::builtins()?;
        let results = ProviderRegistry::new(&catalog).search(
            MODRINTH,
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
        assert_eq!(result.provider, MODRINTH);
        assert_eq!(result.project, "AANobbMI");
        assert_eq!(result.reference, "sodium");
        Ok(())
    }
}
