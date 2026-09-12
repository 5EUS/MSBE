//! Signed declarative provider programs interpreted by reviewed runtimes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{EnvelopeError, ExtensionEnvelope, ExtensionProvide, Provider, VerifyingKey};

/// The only provider-program envelope schema understood by this release.
pub const PROGRAM_SCHEMA_VERSION: u32 = 1;

/// A provider program carried by the common extension envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderProgramEnvelope(pub ExtensionEnvelope<ProviderProgram>);

impl ProviderProgramEnvelope {
    /// Parses and validates an envelope before any runtime is constructed.
    pub fn from_toml(document: &str) -> Result<Self, ProgramError> {
        let envelope: Self = toml::from_str(document).map_err(|error| ProgramError::Parse(error.to_string()))?;
        envelope.validate()?;
        Ok(envelope)
    }

    /// Computes the normalized payload digest registries must place in an envelope.
    pub fn digest_for(program: &ProviderProgram) -> Result<String, ProgramError> {
        Ok(ExtensionEnvelope::package_digest_for(program)?)
    }

    /// Checks structural safety before trust policy is applied by the registry.
    pub fn validate(&self) -> Result<(), ProgramError> {
        self.0.validate()?;
        if !self.0.provides.contains(&ExtensionProvide::ProviderProgramV1) { return Err(ProgramError::UnsupportedSchema(self.0.schema)); }
        self.0.payload.validate()
    }

    /// Verifies this program against a signer-to-verifying-key trust store.
    pub fn verify(&self, trusted_keys: &BTreeMap<String, VerifyingKey>) -> Result<(), ProgramError> { Ok(self.0.verify(trusted_keys)?) }
}

/// The closed vocabulary a reviewed provider runtime may interpret.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProgram {
    /// Provider identity, source recognition, acquisition and policy.
    pub provider: Provider,
    /// The reviewed interpreter selected for this payload.
    pub runtime: RuntimeKind,
    /// Operations this program permits its runtime to expose.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Fixed endpoint-relative routes for catalog runtimes.
    #[serde(default)]
    pub routes: Routes,
    /// JSON pointers which map catalog response data to the neutral model.
    #[serde(default)]
    pub mappings: Mappings,
}

impl ProviderProgram {
    fn validate(&self) -> Result<(), ProgramError> {
        match self.runtime {
            RuntimeKind::DirectUrlV1 if !self.capabilities.is_empty() || !self.routes.is_empty() || !self.mappings.is_empty() => Err(ProgramError::UnexpectedConfiguration("direct-url-v1")),
            RuntimeKind::CatalogV1 if self.provider.api_base().is_none() => Err(ProgramError::MissingMetadata(self.provider.id.clone())),
            RuntimeKind::CatalogV1 => {
                self.routes.validate()?;
                self.mappings.validate()?;
                self.validate_capabilities()
            }
            RuntimeKind::DirectUrlV1 => Ok(()),
        }
    }

    fn validate_capabilities(&self) -> Result<(), ProgramError> {
        for (index, capability) in self.capabilities.iter().enumerate() {
            if self.capabilities[..index].contains(capability) {
                return Err(ProgramError::DuplicateCapability(*capability));
            }
        }

        let search = self.capabilities.contains(&Capability::Search);
        let project = self.capabilities.contains(&Capability::Project);
        let releases = self.capabilities.contains(&Capability::Releases);
        if project != releases {
            return Err(ProgramError::CapabilityDependency {
                capability: if project { Capability::Project } else { Capability::Releases },
                dependency: if project { Capability::Releases } else { Capability::Project },
            });
        }
        if search {
            self.require(self.routes.search.is_some(), Capability::Search, "route search")?;
            self.require(self.mappings.search_items.is_some(), Capability::Search, "mapping search_items")?;
            self.require_project_identity(Capability::Search)?;
        }
        if project {
            self.require(self.routes.project.is_some(), Capability::Project, "route project")?;
            self.require_project_identity(Capability::Project)?;
            self.require(self.routes.releases.is_some(), Capability::Releases, "route releases")?;
            self.require_release_model()?;
        }
        Ok(())
    }

    fn require(&self, present: bool, capability: Capability, requirement: &'static str) -> Result<(), ProgramError> {
        present.then_some(()).ok_or(ProgramError::MissingCapabilityRequirement { capability, requirement })
    }

    fn require_project_identity(&self, capability: Capability) -> Result<(), ProgramError> {
        self.require(self.mappings.project.id.is_some(), capability, "mapping project.id")?;
        self.require(self.mappings.project.title.is_some(), capability, "mapping project.title")
    }

    fn require_release_model(&self) -> Result<(), ProgramError> {
        let release = &self.mappings.release;
        self.require(release.id.is_some(), Capability::Releases, "mapping release.id")?;
        self.require(release.number.is_some(), Capability::Releases, "mapping release.number")?;
        self.require(release.published.is_some(), Capability::Releases, "mapping release.published")?;
        self.require(release.files.is_some(), Capability::Releases, "mapping release.files")?;
        self.require(release.game_versions.is_some(), Capability::Releases, "mapping release.game_versions")?;
        self.require(release.loaders.is_some(), Capability::Releases, "mapping release.loaders")?;
        self.require(release.dependencies.is_some(), Capability::Releases, "mapping release.dependencies")?;
        self.require(release.file.url.is_some(), Capability::Releases, "mapping release.file.url")?;
        self.require(release.file.name.is_some(), Capability::Releases, "mapping release.file.name")?;
        self.require(release.dependency.project.is_some(), Capability::Releases, "mapping release.dependency.project")?;
        self.require(release.dependency.kind.is_some(), Capability::Releases, "mapping release.dependency.kind")
    }
}

/// Reviewed program runtimes; additions require an MSBE code review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    /// Parses one HTTPS URL and optional checksum fragment.
    DirectUrlV1,
    /// Maps bounded JSON catalog responses through explicit pointers.
    CatalogV1,
}

/// Operations a declarative catalog may expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Project search.
    Search,
    /// Project metadata.
    Project,
    /// Release metadata.
    Releases,
}

/// Fixed, endpoint-relative catalog routes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    /// Search route.
    pub search: Option<String>,
    /// Project route containing `{reference}`.
    pub project: Option<String>,
    /// Release route containing `{project}`.
    pub releases: Option<String>,
}
impl Routes {
    const fn is_empty(&self) -> bool { self.search.is_none() && self.project.is_none() && self.releases.is_none() }
    fn validate(&self) -> Result<(), ProgramError> {
        for route in [&self.search, &self.project, &self.releases].into_iter().flatten() {
            if !route.starts_with('/') || route.contains("//") || route.contains("..") || !route.is_ascii() { return Err(ProgramError::InvalidRoute(route.clone())); }
        }
        Ok(())
    }
}

/// Bounded JSON-pointer mappings used by `catalog-v1`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mappings {
    /// Pointer to the array of search objects.
    pub search_items: Option<String>,
    /// Fields in each project object.
    #[serde(default)]
    pub project: ObjectMapping,
    /// Fields in each release object.
    #[serde(default)]
    pub release: ReleaseMapping,
}
impl Mappings {
    const fn is_empty(&self) -> bool { self.search_items.is_none() && self.project.is_empty() && self.release.is_empty() }
    fn validate(&self) -> Result<(), ProgramError> {
        for pointer in self.all_pointers() {
            if pointer.len() > 256 || !valid_json_pointer(pointer) {
                return Err(ProgramError::InvalidPointer(pointer.clone()));
            }
        }
        Ok(())
    }
    fn all_pointers(&self) -> impl Iterator<Item = &String> { self.search_items.iter().chain(self.project.all_pointers()).chain(self.release.all_pointers()) }
}

/// Project fields selected from a JSON object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectMapping {
    /// Project identifier pointer.
    pub id: Option<String>,
    /// Display title pointer.
    pub title: Option<String>,
    /// User-facing reference pointer.
    pub slug: Option<String>,
    /// Search description pointer.
    pub description: Option<String>,
    /// Download count pointer.
    pub downloads: Option<String>,
}
impl ObjectMapping { const fn is_empty(&self) -> bool { self.id.is_none() && self.title.is_none() && self.slug.is_none() && self.description.is_none() && self.downloads.is_none() } fn all_pointers(&self) -> impl Iterator<Item = &String> { [&self.id, &self.title, &self.slug, &self.description, &self.downloads].into_iter().flatten() } }

/// Release fields selected from a JSON object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMapping {
    /// Release identifier pointer.
    pub id: Option<String>,
    /// Release number pointer.
    pub number: Option<String>,
    /// Publication timestamp pointer.
    pub published: Option<String>,
    /// Downloadable file array pointer.
    pub files: Option<String>,
    /// Supported game-version array pointer.
    pub game_versions: Option<String>,
    /// Supported loader array pointer.
    pub loaders: Option<String>,
    /// Dependency array pointer.
    pub dependencies: Option<String>,
    /// Fields in each downloadable file object.
    #[serde(default)]
    pub file: FileMapping,
    /// Fields in each dependency object.
    #[serde(default)]
    pub dependency: DependencyMapping,
}
impl ReleaseMapping {
    const fn is_empty(&self) -> bool { self.id.is_none() && self.number.is_none() && self.published.is_none() && self.files.is_none() && self.game_versions.is_none() && self.loaders.is_none() && self.dependencies.is_none() && self.file.is_empty() && self.dependency.is_empty() }
    fn all_pointers(&self) -> impl Iterator<Item = &String> { [&self.id, &self.number, &self.published, &self.files, &self.game_versions, &self.loaders, &self.dependencies].into_iter().flatten().chain(self.file.all_pointers()).chain(self.dependency.all_pointers()) }
}

/// File fields selected from a release file object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMapping {
    /// Download URL pointer.
    pub url: Option<String>,
    /// File name pointer.
    pub name: Option<String>,
    /// File size pointer.
    pub size: Option<String>,
    /// SHA-256 digest pointer.
    pub sha256: Option<String>,
    /// SHA-512 digest pointer.
    pub sha512: Option<String>,
    /// Primary-file flag pointer.
    pub primary: Option<String>,
}
impl FileMapping {
    const fn is_empty(&self) -> bool { self.url.is_none() && self.name.is_none() && self.size.is_none() && self.sha256.is_none() && self.sha512.is_none() && self.primary.is_none() }
    fn all_pointers(&self) -> impl Iterator<Item = &String> { [&self.url, &self.name, &self.size, &self.sha256, &self.sha512, &self.primary].into_iter().flatten() }
}

/// Relationship fields selected from a release dependency object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyMapping {
    /// Related project identifier pointer.
    pub project: Option<String>,
    /// Related release identifier pointer.
    pub release: Option<String>,
    /// Relationship-kind pointer.
    pub kind: Option<String>,
}
impl DependencyMapping {
    const fn is_empty(&self) -> bool { self.project.is_none() && self.release.is_none() && self.kind.is_none() }
    fn all_pointers(&self) -> impl Iterator<Item = &String> { [&self.project, &self.release, &self.kind].into_iter().flatten() }
}

fn valid_json_pointer(pointer: &str) -> bool {
    let mut bytes = pointer.bytes();
    if pointer.is_empty() {
        return true;
    }
    if bytes.next() != Some(b'/') {
        return false;
    }
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

/// Why an extension program was refused before it could run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProgramError {
    /// TOML decoding failed.
    #[error("invalid provider program: {0}")] Parse(String),
    /// The envelope schema is unsupported.
    #[error("unsupported provider program schema {0}")] UnsupportedSchema(u32),
    /// The signer is not a stable ASCII identity.
    #[error("invalid provider program signer {0:?}")] InvalidSigner(String),
    /// The digest is not SHA-256 hex.
    #[error("invalid provider program digest {0:?}")] InvalidDigest(String),
    /// The content digest did not match.
    #[error("provider program digest mismatch: declared {declared}, computed {actual}")] DigestMismatch {
        /// Declared digest.
        declared: String,
        /// Computed digest.
        actual: String,
    },
    /// Canonical serialization failed.
    #[error("cannot canonicalize provider program: {0}")] Canonical(String),
    /// The containing extension envelope is invalid or untrusted.
    #[error(transparent)] Envelope(#[from] EnvelopeError),
    /// The extension does not provide a provider program interface.
    #[error("extension does not provide provider-program-v1")] MissingProviderProgram,
    /// A runtime received unsupported configuration.
    #[error("{0} does not accept routes, mappings, or capabilities")] UnexpectedConfiguration(&'static str),
    /// A catalog runtime has no API origin.
    #[error("catalog program {0:?} has no metadata endpoint")] MissingMetadata(String),
    /// A route can escape its API origin.
    #[error("invalid provider program route {0:?}")] InvalidRoute(String),
    /// A pointer is outside the supported subset.
    #[error("invalid provider program JSON pointer {0:?}")] InvalidPointer(String),
    /// A capability was listed more than once.
    #[error("duplicate provider program capability {0:?}")] DuplicateCapability(Capability),
    /// A capability cannot operate without another capability.
    #[error("provider program capability {capability:?} requires {dependency:?}")]
    CapabilityDependency {
        /// Capability that cannot stand alone.
        capability: Capability,
        /// Capability required for the runtime to fulfill the first capability.
        dependency: Capability,
    },
    /// A capability lacks a route or mapping required by its reviewed runtime.
    #[error("provider program capability {capability:?} requires {requirement}")]
    MissingCapabilityRequirement {
        /// Capability that cannot be served.
        capability: Capability,
        /// Required route or mapping.
        requirement: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::{ProgramError, ProviderProgram};

    const CATALOG: &str = r#"
        runtime = "catalog-v1"
        capabilities = ["search"]
        [provider]
        schema = 1
        id = "catalog"
        name = "Catalog"
        [provider.source]
        type = "prefixed"
        prefix = "catalog:"
        [provider.metadata]
        api_base = "https://api.example.test"
        [provider.acquisition]
        type = "direct_https"
        [provider.policy]
        requires_auth = false
        respects_distribution_flag = false
        tos_url = ""
        ack_required = false
        [routes]
        search = "/search"
        [mappings]
        search_items = "/hits"
        [mappings.project]
        id = "/id"
        title = "/title"
    "#;

    #[test]
    fn catalog_capabilities_require_their_routes_and_mappings() {
        let program: ProviderProgram = toml::from_str(CATALOG).unwrap();
        assert!(program.validate().is_ok());
        let incomplete = CATALOG.replace("search_items = \"/hits\"\n", "");
        let program: ProviderProgram = toml::from_str(&incomplete).unwrap();
        assert!(matches!(program.validate(), Err(ProgramError::MissingCapabilityRequirement { .. })));
    }

    #[test]
    fn catalog_capabilities_are_unique_and_releases_need_a_complete_model() {
        let duplicate = CATALOG.replace("[\"search\"]", "[\"search\", \"search\"]");
        let program: ProviderProgram = toml::from_str(&duplicate).unwrap();
        assert!(matches!(program.validate(), Err(ProgramError::DuplicateCapability(_))));
        let releases = CATALOG.replace("[\"search\"]", "[\"project\", \"releases\"]");
        let program: ProviderProgram = toml::from_str(&releases).unwrap();
        assert!(matches!(program.validate(), Err(ProgramError::MissingCapabilityRequirement { .. })));
    }

    #[test]
    fn mappings_accept_only_rfc_6901_pointer_syntax() {
        for pointer in ["/a~2b", "/trailing~", "relative"] {
            let invalid = CATALOG.replace("/hits", pointer);
            assert!(matches!(ProviderProgram::validate(&toml::from_str(&invalid).unwrap()), Err(ProgramError::InvalidPointer(_))));
        }
        let escaped = CATALOG.replace("/hits", "/a~0b~1c");
        assert!(ProviderProgram::validate(&toml::from_str(&escaped).unwrap()).is_ok());
    }
}