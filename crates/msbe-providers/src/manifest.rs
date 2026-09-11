//! Declarative provider definitions and their constrained acquisition capabilities.
//!
//! A manifest can describe identity, source recognition, metadata origin, policy, and one of
//! the built-in acquisition primitives. It cannot execute arbitrary code or weaken the
//! transport and policy checks enforced by provider adapters.

use std::collections::BTreeMap;

use serde::Deserialize;
use thiserror::Error;

/// The only provider manifest schema version understood by this release.
pub const SCHEMA_VERSION: u32 = 1;

/// A catalog of validated provider definitions, keyed by stable provider identifier.
#[derive(Debug, Clone)]
pub struct Catalog {
    providers: BTreeMap<String, Provider>,
}

impl Catalog {
    /// Loads the provider definitions shipped with MSBE.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if a built-in definition is invalid. This indicates a build
    /// error in MSBE rather than user-provided input.
    pub fn builtins() -> Result<Self, ManifestError> {
        Self::from_toml(&[
            include_str!("manifests/direct.toml"),
            include_str!("manifests/modrinth.toml"),
        ])
    }

    /// Parses and validates a catalog from independent TOML documents.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError`] if a document is malformed, unsafe, or duplicates an id.
    pub fn from_toml(documents: &[&str]) -> Result<Self, ManifestError> {
        let mut providers: BTreeMap<String, Provider> = BTreeMap::new();
        for document in documents {
            let provider: Provider = toml::from_str(document)
                .map_err(|error| ManifestError::Parse(error.to_string()))?;
            provider.validate()?;
            if let Some(existing) = providers
                .values()
                .find(|existing| existing.source.overlaps(&provider.source))
            {
                return Err(ManifestError::AmbiguousSource {
                    first: existing.id.clone(),
                    second: provider.id,
                });
            }
            if providers
                .insert(provider.id.clone(), provider.clone())
                .is_some()
            {
                return Err(ManifestError::DuplicateId(provider.id));
            }
        }
        Ok(Self { providers })
    }

    /// Finds the provider which owns a user-entered source.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::UnknownSource`] when no declared source recognizer accepts it.
    pub fn source<'catalog, 'raw>(
        &'catalog self,
        raw: &'raw str,
    ) -> Result<Source<'catalog, 'raw>, ManifestError> {
        let provider = self
            .providers
            .values()
            .find(|provider| provider.source.matches(raw))
            .ok_or_else(|| ManifestError::UnknownSource(raw.to_owned()))?;
        Ok(Source { provider, raw })
    }

    /// Returns a provider by its stable identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::UnknownProvider`] when the catalog does not contain `id`.
    pub fn provider(&self, id: &str) -> Result<&Provider, ManifestError> {
        self.providers
            .get(id)
            .ok_or_else(|| ManifestError::UnknownProvider(id.to_owned()))
    }
}

/// A source recognized by a catalog provider.
#[derive(Debug, Clone, Copy)]
pub struct Source<'catalog, 'raw> {
    provider: &'catalog Provider,
    raw: &'raw str,
}

impl<'catalog, 'raw> Source<'catalog, 'raw> {
    /// The definition that recognized this source.
    pub const fn provider(&self) -> &'catalog Provider {
        self.provider
    }

    /// The original source text.
    pub const fn raw(&self) -> &'raw str {
        self.raw
    }

    /// The provider-specific source reference, without a declared prefix.
    pub fn reference(&self) -> &'raw str {
        self.provider.source.reference(self.raw)
    }
}

/// A declarative description of one provider.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The manifest schema version.
    pub schema: u32,
    /// A stable, registry-wide provider identifier.
    pub id: String,
    /// The provider name displayed to users.
    pub name: String,
    /// How the provider recognizes a source entered by a user.
    pub source: SourceMatcher,
    /// The provider metadata endpoint, when it exposes one.
    #[serde(default)]
    pub metadata: Option<Metadata>,
    /// The only acquisition primitive this provider may use.
    pub acquisition: Acquisition,
    /// The constraints MSBE must enforce for this provider.
    pub policy: Policy,
}

impl Provider {
    /// Returns the metadata API base URL, when the provider declares one.
    pub fn api_base(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.api_base.as_str())
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.schema != SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(self.schema));
        }
        validate_text("provider id", &self.id)?;
        validate_text("provider name", &self.name)?;
        if !self.id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        }) {
            return Err(ManifestError::InvalidId(self.id.clone()));
        }
        self.source.validate()?;
        if let Some(metadata) = &self.metadata {
            validate_https_url(&metadata.api_base).map_err(|error| {
                ManifestError::InvalidMetadata(metadata.api_base.clone(), error)
            })?;
        }
        if !self.policy.tos_url.is_empty() {
            validate_https_url(&self.policy.tos_url).map_err(|error| {
                ManifestError::InvalidTermsUrl(self.policy.tos_url.clone(), error)
            })?;
        }
        if !matches!(self.acquisition, Acquisition::DirectHttps) && self.metadata.is_some() {
            return Err(ManifestError::UnsupportedAcquisition(self.id.clone()));
        }
        Ok(())
    }
}

/// A source-recognition rule selected from a closed set.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceMatcher {
    /// A provider-specific prefix such as `modrinth:`.
    Prefixed {
        /// The prefix removed before passing the reference to the provider adapter.
        prefix: String,
    },
    /// An arbitrary HTTPS URL, used by the direct URL primitive.
    HttpsUrl,
}

impl SourceMatcher {
    fn matches(&self, raw: &str) -> bool {
        match self {
            Self::Prefixed { prefix } => raw.starts_with(prefix),
            Self::HttpsUrl => raw.starts_with("https://") || raw.starts_with("http://"),
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Prefixed { prefix: left }, Self::Prefixed { prefix: right }) => {
                left.starts_with(right) || right.starts_with(left)
            }
            (Self::HttpsUrl, Self::HttpsUrl) => true,
            (Self::Prefixed { prefix }, Self::HttpsUrl)
            | (Self::HttpsUrl, Self::Prefixed { prefix }) => {
                prefix.starts_with("https://") || prefix.starts_with("http://")
            }
        }
    }

    fn reference<'a>(&self, raw: &'a str) -> &'a str {
        match self {
            Self::Prefixed { prefix } => raw.strip_prefix(prefix).unwrap_or(raw),
            Self::HttpsUrl => raw,
        }
    }

    fn validate(&self) -> Result<(), ManifestError> {
        match self {
            Self::Prefixed { prefix } if prefix.is_empty() => Err(ManifestError::EmptyPrefix),
            Self::Prefixed { prefix } if !prefix.is_ascii() => {
                Err(ManifestError::NonAsciiPrefix(prefix.clone()))
            }
            Self::Prefixed { .. } | Self::HttpsUrl => Ok(()),
        }
    }
}

/// A provider metadata endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// The HTTPS API base URL.
    pub api_base: String,
}

/// The closed acquisition primitive declared by a provider.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Acquisition {
    /// MSBE can fetch HTTPS artifacts after a reviewed adapter supplies their URLs and hashes.
    DirectHttps,
}

/// Policy data declared with a provider definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Whether the provider requires user authentication.
    pub requires_auth: bool,
    /// Whether provider-specific distribution controls must be consulted by an adapter.
    pub respects_distribution_flag: bool,
    /// The provider terms URL, if it has one.
    pub tos_url: String,
    /// Whether the user must explicitly acknowledge the provider policy.
    pub ack_required: bool,
}

/// Why a provider manifest or source could not be used.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// TOML could not be decoded.
    #[error("invalid provider manifest: {0}")]
    Parse(String),
    /// A manifest uses a newer or older schema.
    #[error("unsupported provider manifest schema {0}")]
    UnsupportedSchema(u32),
    /// A required text field is empty or whitespace.
    #[error("{0} must not be empty")]
    EmptyText(&'static str),
    /// An id is not stable and portable.
    #[error("provider id {0:?} must contain only lowercase ASCII letters, digits, or hyphens")]
    InvalidId(String),
    /// A prefix accepts every input.
    #[error("provider source prefix must not be empty")]
    EmptyPrefix,
    /// A prefix cannot be represented consistently by all clients.
    #[error("provider source prefix {0:?} must be ASCII")]
    NonAsciiPrefix(String),
    /// Metadata is not an absolute HTTPS URL with a host.
    #[error("provider metadata endpoint {0:?} is invalid: {1}")]
    InvalidMetadata(String, &'static str),
    /// Provider terms are not an absolute HTTPS URL with a host.
    #[error("provider terms URL {0:?} is invalid: {1}")]
    InvalidTermsUrl(String, &'static str),
    /// The manifest selects an acquisition primitive not available in this release.
    #[error("provider {0:?} selects an unsupported acquisition primitive")]
    UnsupportedAcquisition(String),
    /// Two documents claim the same stable identifier.
    #[error("duplicate provider id {0:?}")]
    DuplicateId(String),
    /// Two source recognizers could claim the same user-entered source.
    #[error("provider source matchers for {first:?} and {second:?} overlap")]
    AmbiguousSource {
        /// The first provider id.
        first: String,
        /// The second provider id.
        second: String,
    },
    /// No manifest recognizes a user-entered source.
    #[error("no provider recognizes source {0:?}")]
    UnknownSource(String),
    /// A named provider is absent from the catalog.
    #[error("unknown provider {0:?}")]
    UnknownProvider(String),
    /// A provider does not declare the metadata endpoint required by its adapter.
    #[error("provider {0:?} does not declare a metadata endpoint")]
    MissingMetadata(String),
}

fn validate_text(field: &'static str, value: &str) -> Result<(), ManifestError> {
    if value.trim().is_empty() {
        Err(ManifestError::EmptyText(field))
    } else {
        Ok(())
    }
}

fn validate_https_url(url: &str) -> Result<(), &'static str> {
    let authority = url.strip_prefix("https://").ok_or("must use https")?;
    let host = authority
        .split(['/', '?', '#'])
        .next()
        .filter(|host| !host.is_empty() && !host.contains(char::is_whitespace))
        .ok_or("must include a host")?;
    if host.contains('@') {
        return Err("must not include user info");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Catalog, ManifestError};

    #[test]
    fn builtins_recognize_their_declared_sources() -> Result<(), ManifestError> {
        let catalog = Catalog::builtins()?;
        let modrinth = catalog.source("modrinth:sodium")?;
        assert_eq!(modrinth.provider().id, "modrinth");
        assert_eq!(modrinth.reference(), "sodium");
        assert_eq!(
            catalog
                .source("https://example.test/mod.jar")?
                .provider()
                .id,
            "url"
        );
        Ok(())
    }

    #[test]
    fn rejects_an_insecure_metadata_endpoint() {
        let manifest = r#"
            schema = 1
            id = "example"
            name = "Example"
            [source]
            type = "prefixed"
            prefix = "example:"
            [metadata]
            api_base = "http://example.test/api"
            [acquisition]
            type = "direct_https"
            [policy]
            requires_auth = false
            respects_distribution_flag = false
            tos_url = ""
            ack_required = false
        "#;
        assert!(matches!(
            Catalog::from_toml(&[manifest]),
            Err(ManifestError::InvalidMetadata(_, "must use https"))
        ));
    }

    #[test]
    fn rejects_malformed_metadata_and_terms_urls() {
        let malformed_metadata = r#"
            schema = 1
            id = "example"
            name = "Example"
            [source]
            type = "prefixed"
            prefix = "example:"
            [metadata]
            api_base = "https:///api"
            [acquisition]
            type = "direct_https"
            [policy]
            requires_auth = false
            respects_distribution_flag = false
            tos_url = ""
            ack_required = false
        "#;
        assert!(matches!(
            Catalog::from_toml(&[malformed_metadata]),
            Err(ManifestError::InvalidMetadata(_, "must include a host"))
        ));

        let malformed_terms = malformed_metadata
            .replace(
                "api_base = \"https:///api\"",
                "api_base = \"https://api.example.test\"",
            )
            .replace("tos_url = \"\"", "tos_url = \"http://terms.example.test\"");
        assert!(matches!(
            Catalog::from_toml(&[&malformed_terms]),
            Err(ManifestError::InvalidTermsUrl(_, "must use https"))
        ));
    }

    #[test]
    fn rejects_overlapping_source_prefixes() {
        let manifests = [
            r#"
                schema = 1
                id = "first"
                name = "First"
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
            "#,
            r#"
                schema = 1
                id = "second"
                name = "Second"
                [source]
                type = "prefixed"
                prefix = "example:mod:"
                [acquisition]
                type = "direct_https"
                [policy]
                requires_auth = false
                respects_distribution_flag = false
                tos_url = ""
                ack_required = false
            "#,
        ];
        assert!(matches!(
            Catalog::from_toml(&manifests),
            Err(ManifestError::AmbiguousSource { .. })
        ));
    }
}
