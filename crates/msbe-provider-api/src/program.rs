//! Signed declarative provider programs interpreted by reviewed runtimes.
//!
//! A program composes closed vocabulary: it selects a runtime MSBE reviewed, and fills that
//! runtime's slots with fixed routes, named request parameters, and JSON pointers into responses.
//! It never supplies code, expressions, or arbitrary requests. How target facts are encoded, how
//! releases are filtered and ordered, and which release may replace an installed one belong to the
//! runtime. See `docs/06-providers-and-policy.md` §6.4.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{EnvelopeError, ExtensionEnvelope, ExtensionProvide, Provider, VerifyingKey};

/// The only provider-program envelope schema understood by this release.
pub const PROGRAM_SCHEMA_VERSION: u32 = 1;

/// The longest route, parameter name, template, or literal a program may declare.
const TEXT_LIMIT: usize = 256;

/// A provider program carried by the common extension envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderProgramEnvelope(pub ExtensionEnvelope<ProviderProgram>);

impl ProviderProgramEnvelope {
    /// Parses and validates an envelope before any runtime is constructed.
    ///
    /// # Errors
    ///
    /// Returns [`ProgramError`] when the document does not parse or fails validation.
    pub fn from_toml(document: &str) -> Result<Self, ProgramError> {
        let envelope: Self =
            toml::from_str(document).map_err(|error| ProgramError::Parse(error.to_string()))?;
        envelope.validate()?;
        Ok(envelope)
    }

    /// Computes the normalized payload digest registries must place in an envelope.
    ///
    /// # Errors
    ///
    /// Returns [`ProgramError`] when the program cannot be serialized canonically.
    pub fn digest_for(program: &ProviderProgram) -> Result<String, ProgramError> {
        Ok(ExtensionEnvelope::package_digest_for(program)?)
    }

    /// Checks structural safety before trust policy is applied by the registry.
    ///
    /// # Errors
    ///
    /// Returns [`ProgramError`] naming the first envelope or vocabulary rule that fails.
    pub fn validate(&self) -> Result<(), ProgramError> {
        self.0.validate()?;
        if !self
            .0
            .provides
            .contains(&ExtensionProvide::ProviderProgramV1)
        {
            return Err(ProgramError::UnsupportedSchema(self.0.schema));
        }
        self.0.payload.validate()
    }

    /// Verifies this program against a signer-to-verifying-key trust store.
    ///
    /// # Errors
    ///
    /// Returns [`ProgramError`] when the signer is untrusted or the signature does not verify.
    pub fn verify(
        &self,
        trusted_keys: &BTreeMap<String, VerifyingKey>,
    ) -> Result<(), ProgramError> {
        Ok(self.0.verify(trusted_keys)?)
    }
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
    /// How a catalog is asked to search.
    #[serde(default)]
    pub search: SearchRequest,
    /// How a catalog is asked for a project's releases, and the order they are offered in.
    #[serde(default)]
    pub releases: ReleasesRequest,
    /// How a catalog is asked for updates, for the `updates` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<UpdateProtocol>,
    /// JSON pointers which map catalog response data to the neutral model.
    #[serde(default)]
    pub mappings: Mappings,
}

impl ProviderProgram {
    /// Checks the program against its runtime's closed vocabulary, before any runtime is built.
    ///
    /// # Errors
    ///
    /// Returns [`ProgramError`] naming the first rule the program breaks.
    pub fn validate(&self) -> Result<(), ProgramError> {
        match self.runtime {
            RuntimeKind::DirectUrlV1 => {
                let unconfigured = self.capabilities.is_empty()
                    && self.routes.is_empty()
                    && self.mappings.is_empty()
                    && self.search == SearchRequest::default()
                    && self.releases == ReleasesRequest::default()
                    && self.updates.is_none();
                if unconfigured {
                    Ok(())
                } else {
                    Err(ProgramError::UnexpectedConfiguration("direct-url-v1"))
                }
            }
            RuntimeKind::CatalogV1 => {
                if self.provider.api_base().is_none() {
                    return Err(ProgramError::MissingMetadata(self.provider.id.clone()));
                }
                self.routes.validate()?;
                self.search.validate()?;
                self.releases.validate()?;
                if let Some(updates) = &self.updates {
                    updates.validate()?;
                }
                self.mappings.validate()?;
                self.validate_capabilities()
            }
        }
    }

    fn validate_capabilities(&self) -> Result<(), ProgramError> {
        for (index, capability) in self.capabilities.iter().enumerate() {
            if self
                .capabilities
                .iter()
                .take(index)
                .any(|earlier| earlier == capability)
            {
                return Err(ProgramError::DuplicateCapability(*capability));
            }
        }
        let has = |capability| self.capabilities.contains(&capability);
        let (project, releases) = (has(Capability::Project), has(Capability::Releases));
        if project != releases {
            return Err(ProgramError::CapabilityDependency {
                capability: if project {
                    Capability::Project
                } else {
                    Capability::Releases
                },
                dependency: if project {
                    Capability::Releases
                } else {
                    Capability::Project
                },
            });
        }
        if has(Capability::Search) {
            self.require_search()?;
        }
        if project {
            Self::require(
                self.routes.project.is_some(),
                Capability::Project,
                "route project",
            )?;
            self.require_project_identity(Capability::Project)?;
            Self::require(
                self.routes.releases.is_some(),
                Capability::Releases,
                "route releases",
            )?;
            self.require_release_model(Capability::Releases)?;
        }
        if has(Capability::ReleaseProject) {
            if !releases {
                return Err(ProgramError::CapabilityDependency {
                    capability: Capability::ReleaseProject,
                    dependency: Capability::Releases,
                });
            }
            Self::require(
                self.routes.release.is_some(),
                Capability::ReleaseProject,
                "route release",
            )?;
            Self::require(
                self.mappings.release.project.is_some(),
                Capability::ReleaseProject,
                "mapping release.project",
            )?;
        }
        match (has(Capability::Updates), self.updates.is_some()) {
            (true, false) => Err(ProgramError::MissingCapabilityRequirement {
                capability: Capability::Updates,
                requirement: "section updates",
            }),
            (false, true) => Err(ProgramError::UnusedSection("updates")),
            (true, true) => {
                self.require_release_model(Capability::Updates)?;
                Self::require(
                    self.mappings.release.project.is_some(),
                    Capability::Updates,
                    "mapping release.project",
                )?;
                Self::require(
                    self.mappings.release.channel.is_some(),
                    Capability::Updates,
                    "mapping release.channel",
                )
            }
            (false, false) => Ok(()),
        }
    }

    fn require_search(&self) -> Result<(), ProgramError> {
        Self::require(
            self.routes.search.is_some(),
            Capability::Search,
            "route search",
        )?;
        Self::require(
            self.mappings.search_items.is_some(),
            Capability::Search,
            "mapping search_items",
        )?;
        let hit = self.mappings.hit.as_ref().unwrap_or(&self.mappings.project);
        Self::require(
            hit.id.is_some(),
            Capability::Search,
            "mapping hit.id or project.id",
        )?;
        Self::require(
            hit.title.is_some(),
            Capability::Search,
            "mapping hit.title or project.title",
        )
    }

    fn require(
        present: bool,
        capability: Capability,
        requirement: &'static str,
    ) -> Result<(), ProgramError> {
        present
            .then_some(())
            .ok_or(ProgramError::MissingCapabilityRequirement {
                capability,
                requirement,
            })
    }

    fn require_project_identity(&self, capability: Capability) -> Result<(), ProgramError> {
        Self::require(
            self.mappings.project.id.is_some(),
            capability,
            "mapping project.id",
        )?;
        Self::require(
            self.mappings.project.title.is_some(),
            capability,
            "mapping project.title",
        )
    }

    fn require_release_model(&self, capability: Capability) -> Result<(), ProgramError> {
        let release = &self.mappings.release;
        for (present, requirement) in [
            (release.id.is_some(), "mapping release.id"),
            (release.number.is_some(), "mapping release.number"),
            (release.published.is_some(), "mapping release.published"),
            (release.files.is_some(), "mapping release.files"),
            (
                release.game_versions.is_some(),
                "mapping release.game_versions",
            ),
            (release.loaders.is_some(), "mapping release.loaders"),
            (
                release.dependencies.is_some(),
                "mapping release.dependencies",
            ),
            (release.file.url.is_some(), "mapping release.file.url"),
            (release.file.name.is_some(), "mapping release.file.name"),
            (
                release.dependency.project.is_some(),
                "mapping release.dependency.project",
            ),
            (
                release.dependency.kind.is_some(),
                "mapping release.dependency.kind",
            ),
        ] {
            Self::require(present, capability, requirement)?;
        }
        Ok(())
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
    /// The project a release belongs to, for a dependency that names only a release.
    ReleaseProject,
    /// Update checks for installed files.
    Updates,
}

/// Fixed, endpoint-relative catalog routes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    /// Search route.
    pub search: Option<String>,
    /// Project route containing `{reference}`.
    pub project: Option<String>,
    /// Release listing route containing `{project}`.
    pub releases: Option<String>,
    /// Single-release route containing `{release}`.
    pub release: Option<String>,
}

impl Routes {
    const fn is_empty(&self) -> bool {
        self.search.is_none()
            && self.project.is_none()
            && self.releases.is_none()
            && self.release.is_none()
    }

    fn validate(&self) -> Result<(), ProgramError> {
        for (route, placeholder) in [
            (&self.search, None),
            (&self.project, Some("reference")),
            (&self.releases, Some("project")),
            (&self.release, Some("release")),
        ] {
            if let Some(route) = route {
                check_route(route, placeholder)?;
            }
        }
        Ok(())
    }
}

/// How a catalog is asked to search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    /// The query parameter carrying the search words.
    #[serde(default = "default_query")]
    pub query: String,
    /// The query parameter carrying the most results to return.
    #[serde(default = "default_limit")]
    pub limit: String,
    /// The most results the catalog returns at once; a larger limit is lowered to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<u8>,
    /// Target facts sent as grouped filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facets: Option<Facets>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: default_query(),
            limit: default_limit(),
            maximum: None,
            facets: None,
        }
    }
}

fn default_query() -> String {
    "q".to_owned()
}

fn default_limit() -> String {
    "limit".to_owned()
}

impl SearchRequest {
    fn validate(&self) -> Result<(), ProgramError> {
        if self.maximum == Some(0) {
            return Err(ProgramError::InvalidLimit);
        }
        let mut names = vec![self.query.as_str(), self.limit.as_str()];
        if let Some(facets) = &self.facets {
            names.push(&facets.parameter);
            if facets.groups.is_empty() || facets.groups.iter().any(Vec::is_empty) {
                return Err(ProgramError::InvalidTemplate(
                    "facet groups must not be empty".to_owned(),
                ));
            }
            for template in facets.groups.iter().flatten() {
                check_template(template)?;
            }
        }
        check_names(&names)
    }
}

/// Grouped filters a search sends as one query parameter: a JSON array of arrays of strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facets {
    /// The query parameter carrying the groups.
    pub parameter: String,
    /// Groups of templates. A template may hold one placeholder: `{game_version}`, or `{loader}`,
    /// which repeats the template once for every loader the target accepts. A group that expands
    /// to nothing is left out.
    pub groups: Vec<Vec<String>>,
}

/// How a catalog is asked for a project's releases.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasesRequest {
    /// Query parameters sent with every release listing.
    #[serde(default)]
    pub query: Vec<QueryParameter>,
    /// The order compatible releases are offered in.
    #[serde(default)]
    pub order: ReleaseOrder,
}

impl ReleasesRequest {
    fn validate(&self) -> Result<(), ProgramError> {
        for parameter in &self.query {
            let valid = match (&parameter.literal, parameter.target) {
                (Some(literal), None) => literal.is_ascii() && literal.len() <= TEXT_LIMIT,
                (None, Some(_)) => true,
                _ => false,
            };
            if !valid {
                return Err(ProgramError::InvalidParameter(parameter.name.clone()));
            }
        }
        let names: Vec<&str> = self
            .query
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect();
        check_names(&names)
    }
}

/// One query parameter: a fixed literal, or a target fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryParameter {
    /// The parameter name.
    pub name: String,
    /// A fixed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<String>,
    /// A target fact, sent as a JSON array of strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetFact>,
}

/// A fact about the target a request may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetFact {
    /// Every loader the target accepts: its own and those it provides.
    Loaders,
    /// The target's game version.
    GameVersion,
}

/// The order compatible releases are offered in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseOrder {
    /// As the catalog lists them.
    #[default]
    Listed,
    /// Newest publication first, whatever the channel.
    NewestFirst,
}

/// How a catalog is asked for updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProtocol {
    /// The reviewed protocol.
    #[serde(rename = "type")]
    pub kind: UpdateProtocolKind,
    /// The published file hash installed files are looked up by.
    pub algorithm: HashAlgorithm,
    /// Route answering which release each hashed file belongs to.
    pub listed: String,
    /// Route answering the newest release, admitted by filters, of each hashed file's project.
    pub latest: String,
    /// The request body's field names.
    pub fields: UpdateFields,
}

impl UpdateProtocol {
    fn validate(&self) -> Result<(), ProgramError> {
        check_route(&self.listed, None)?;
        check_route(&self.latest, None)?;
        let fields = &self.fields;
        check_names(&[
            &fields.hashes,
            &fields.algorithm,
            &fields.loaders,
            &fields.game_versions,
            &fields.channels,
        ])
    }
}

/// Reviewed update protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateProtocolKind {
    /// Bulk lookups by file hash, one for the installed releases and one per release channel in
    /// use for the newest compatible release. A file stays on its channel or moves to a more stable
    /// one, and moves to an older release only to regain compatibility with the target.
    HashLookupV1,
}

/// A published file hash algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    /// SHA-256.
    Sha256,
    /// SHA-512.
    Sha512,
}

impl HashAlgorithm {
    /// The algorithm's name, as provenance records and catalogs spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }
}

/// The field names of an update request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFields {
    /// The hashes looked up.
    pub hashes: String,
    /// The hash algorithm.
    pub algorithm: String,
    /// The loaders a replacement must support.
    pub loaders: String,
    /// The game versions a replacement must support.
    pub game_versions: String,
    /// The release channels a replacement may be on.
    pub channels: String,
}

/// Bounded JSON-pointer mappings used by `catalog-v1`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mappings {
    /// Pointer to the array of search objects.
    pub search_items: Option<String>,
    /// Fields in each search object, when they differ from a project's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit: Option<ObjectMapping>,
    /// Fields in each project object.
    #[serde(default)]
    pub project: ObjectMapping,
    /// Fields in each release object.
    #[serde(default)]
    pub release: ReleaseMapping,
}

impl Mappings {
    const fn is_empty(&self) -> bool {
        self.search_items.is_none()
            && self.hit.is_none()
            && self.project.is_empty()
            && self.release.is_empty()
    }

    fn validate(&self) -> Result<(), ProgramError> {
        for pointer in self.all_pointers() {
            if pointer.len() > TEXT_LIMIT || !valid_json_pointer(pointer) {
                return Err(ProgramError::InvalidPointer(pointer.clone()));
            }
        }
        if let Some(channel) = &self.release.channel {
            let values = [&channel.release, &channel.beta, &channel.alpha];
            let distinct = values
                .iter()
                .enumerate()
                .all(|(index, value)| !values.iter().take(index).any(|earlier| earlier == value));
            if !distinct
                || values
                    .iter()
                    .any(|value| value.is_empty() || !value.is_ascii())
            {
                return Err(ProgramError::InvalidChannels);
            }
        }
        Ok(())
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        self.search_items
            .iter()
            .chain(self.hit.iter().flat_map(ObjectMapping::all_pointers))
            .chain(self.project.all_pointers())
            .chain(self.release.all_pointers())
    }
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
    /// Icon URL pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Player-client availability pointer: `required`, `optional`, `unsupported` or `unknown`.
    /// Unmapped, a project is taken as optional on clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Dedicated-server availability pointer, with the values `client` takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl ObjectMapping {
    const fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.title.is_none()
            && self.slug.is_none()
            && self.description.is_none()
            && self.downloads.is_none()
            && self.icon.is_none()
            && self.client.is_none()
            && self.server.is_none()
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        [
            &self.id,
            &self.title,
            &self.slug,
            &self.description,
            &self.downloads,
            &self.icon,
            &self.client,
            &self.server,
        ]
        .into_iter()
        .flatten()
    }
}

/// Release fields selected from a JSON object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMapping {
    /// Release identifier pointer.
    pub id: Option<String>,
    /// Pointer to the identifier of the project the release belongs to. Unmapped, a listed
    /// release belongs to the project that was listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
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
    /// Pointer to an object of supported loader-version arrays, keyed by loader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_versions: Option<String>,
    /// Dependency array pointer.
    pub dependencies: Option<String>,
    /// The release channel, and how the catalog names each channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelMapping>,
    /// Fields in each downloadable file object.
    #[serde(default)]
    pub file: FileMapping,
    /// Fields in each dependency object.
    #[serde(default)]
    pub dependency: DependencyMapping,
}

impl ReleaseMapping {
    const fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.project.is_none()
            && self.number.is_none()
            && self.published.is_none()
            && self.files.is_none()
            && self.game_versions.is_none()
            && self.loaders.is_none()
            && self.loader_versions.is_none()
            && self.dependencies.is_none()
            && self.channel.is_none()
            && self.file.is_empty()
            && self.dependency.is_empty()
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        [
            &self.id,
            &self.project,
            &self.number,
            &self.published,
            &self.files,
            &self.game_versions,
            &self.loaders,
            &self.loader_versions,
            &self.dependencies,
        ]
        .into_iter()
        .flatten()
        .chain(self.channel.iter().map(|channel| &channel.pointer))
        .chain(self.file.all_pointers())
        .chain(self.dependency.all_pointers())
    }
}

/// Where a release's channel is, and the catalog's name for each channel. Any other value is an
/// unknown channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelMapping {
    /// Channel pointer.
    pub pointer: String,
    /// The catalog's name for stable releases.
    pub release: String,
    /// The catalog's name for pre-releases.
    pub beta: String,
    /// The catalog's name for early, unstable builds.
    pub alpha: String,
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
    /// Primary-file flag pointer. Unmapped, every file is primary; mapped, a file without the flag
    /// is not.
    pub primary: Option<String>,
}

impl FileMapping {
    const fn is_empty(&self) -> bool {
        self.url.is_none()
            && self.name.is_none()
            && self.size.is_none()
            && self.sha256.is_none()
            && self.sha512.is_none()
            && self.primary.is_none()
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        [
            &self.url,
            &self.name,
            &self.size,
            &self.sha256,
            &self.sha512,
            &self.primary,
        ]
        .into_iter()
        .flatten()
    }
}

/// Relationship fields selected from a release dependency object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyMapping {
    /// Related project identifier pointer. A dependency without one names only a release.
    pub project: Option<String>,
    /// Related release identifier pointer.
    pub release: Option<String>,
    /// Relationship-kind pointer: `required`, `optional`, `incompatible` or `embedded`.
    pub kind: Option<String>,
}

impl DependencyMapping {
    const fn is_empty(&self) -> bool {
        self.project.is_none() && self.release.is_none() && self.kind.is_none()
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        [&self.project, &self.release, &self.kind]
            .into_iter()
            .flatten()
    }
}

/// Refuses a route that can escape its origin, or whose placeholders are not exactly
/// `placeholder`, once.
fn check_route(route: &str, placeholder: Option<&str>) -> Result<(), ProgramError> {
    let invalid = || ProgramError::InvalidRoute(route.to_owned());
    if !route.starts_with('/')
        || route.contains("//")
        || route.contains("..")
        || !route.is_ascii()
        || route.len() > TEXT_LIMIT
    {
        return Err(invalid());
    }
    let rest = match placeholder {
        Some(name) => {
            let marker = format!("{{{name}}}");
            if route.matches(&marker).count() != 1 {
                return Err(invalid());
            }
            route.replacen(&marker, "", 1)
        }
        None => route.to_owned(),
    };
    if rest.contains(['{', '}', '?', '#', ' ']) {
        return Err(invalid());
    }
    Ok(())
}

/// Refuses a facet template with an unknown placeholder, or more than one.
fn check_template(template: &str) -> Result<(), ProgramError> {
    let markers = template.matches("{game_version}").count() + template.matches("{loader}").count();
    let rest = template
        .replacen("{game_version}", "", 1)
        .replacen("{loader}", "", 1);
    if template.is_empty()
        || !template.is_ascii()
        || template.len() > TEXT_LIMIT
        || markers > 1
        || rest.contains(['{', '}'])
    {
        return Err(ProgramError::InvalidTemplate(template.to_owned()));
    }
    Ok(())
}

/// Refuses parameter or field names that are malformed or repeated.
fn check_names(names: &[&str]) -> Result<(), ProgramError> {
    for (index, name) in names.iter().enumerate() {
        let well_formed = !name.is_empty()
            && name.len() <= TEXT_LIMIT
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        if !well_formed || names.iter().take(index).any(|earlier| earlier == name) {
            return Err(ProgramError::InvalidParameter((*name).to_owned()));
        }
    }
    Ok(())
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
    #[error("invalid provider program: {0}")]
    Parse(String),
    /// The envelope schema is unsupported.
    #[error("unsupported provider program schema {0}")]
    UnsupportedSchema(u32),
    /// The signer is not a stable ASCII identity.
    #[error("invalid provider program signer {0:?}")]
    InvalidSigner(String),
    /// The digest is not SHA-256 hex.
    #[error("invalid provider program digest {0:?}")]
    InvalidDigest(String),
    /// The content digest did not match.
    #[error("provider program digest mismatch: declared {declared}, computed {actual}")]
    DigestMismatch {
        /// Declared digest.
        declared: String,
        /// Computed digest.
        actual: String,
    },
    /// Canonical serialization failed.
    #[error("cannot canonicalize provider program: {0}")]
    Canonical(String),
    /// The containing extension envelope is invalid or untrusted.
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// The extension does not provide a provider program interface.
    #[error("extension does not provide provider-program-v1")]
    MissingProviderProgram,
    /// A runtime received unsupported configuration.
    #[error("{0} does not accept routes, requests, mappings, or capabilities")]
    UnexpectedConfiguration(&'static str),
    /// A section is declared without the capability that uses it.
    #[error("provider program declares [{0}] without the capability that uses it")]
    UnusedSection(&'static str),
    /// A catalog runtime has no API origin.
    #[error("catalog program {0:?} has no metadata endpoint")]
    MissingMetadata(String),
    /// A route can escape its API origin.
    #[error("invalid provider program route {0:?}")]
    InvalidRoute(String),
    /// A pointer is outside the supported subset.
    #[error("invalid provider program JSON pointer {0:?}")]
    InvalidPointer(String),
    /// A request parameter or field name is malformed, repeated, or has no single value.
    #[error("invalid provider program request parameter {0:?}")]
    InvalidParameter(String),
    /// A facet template has an unknown placeholder, or more than one.
    #[error("invalid provider program template {0:?}")]
    InvalidTemplate(String),
    /// A search maximum of zero.
    #[error("a provider program search maximum must be at least 1")]
    InvalidLimit,
    /// Channel names that are empty or not distinct.
    #[error("provider program channel names must be distinct and non-empty")]
    InvalidChannels,
    /// A capability was listed more than once.
    #[error("duplicate provider program capability {0:?}")]
    DuplicateCapability(Capability),
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

    /// A catalog with every capability, in the shape of a real catalog API.
    const COMPLETE: &str = r#"
        runtime = "catalog-v1"
        capabilities = ["search", "project", "releases", "release-project", "updates"]
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
        project = "/project/{reference}"
        releases = "/project/{project}/version"
        release = "/version/{release}"
        [search]
        query = "query"
        maximum = 100
        facets = { parameter = "facets", groups = [["versions:{game_version}"], ["categories:{loader}"]] }
        [releases]
        order = "newest-first"
        query = [{ name = "loaders", target = "loaders" }, { name = "stable", literal = "true" }]
        [updates]
        type = "hash-lookup-v1"
        algorithm = "sha512"
        listed = "/files"
        latest = "/files/latest"
        fields = { hashes = "hashes", algorithm = "algorithm", loaders = "loaders", game_versions = "game_versions", channels = "channels" }
        [mappings]
        search_items = "/hits"
        [mappings.hit]
        id = "/project_id"
        title = "/title"
        [mappings.project]
        id = "/id"
        title = "/title"
        [mappings.release]
        id = "/id"
        project = "/project_id"
        number = "/number"
        published = "/published"
        files = "/files"
        game_versions = "/game_versions"
        loaders = "/loaders"
        dependencies = "/dependencies"
        channel = { pointer = "/channel", release = "stable", beta = "beta", alpha = "alpha" }
        [mappings.release.file]
        url = "/url"
        name = "/name"
        [mappings.release.dependency]
        project = "/project_id"
        kind = "/kind"
    "#;

    fn validate(document: &str) -> Result<(), ProgramError> {
        toml::from_str::<ProviderProgram>(document)
            .unwrap()
            .validate()
    }

    #[test]
    fn catalog_capabilities_require_their_routes_and_mappings() {
        assert!(validate(CATALOG).is_ok());
        let incomplete = CATALOG.replace("search_items = \"/hits\"\n", "");
        assert!(matches!(
            validate(&incomplete),
            Err(ProgramError::MissingCapabilityRequirement { .. })
        ));
    }

    #[test]
    fn catalog_capabilities_are_unique_and_releases_need_a_complete_model() {
        let duplicate = CATALOG.replace("[\"search\"]", "[\"search\", \"search\"]");
        assert!(matches!(
            validate(&duplicate),
            Err(ProgramError::DuplicateCapability(_))
        ));
        let releases = CATALOG.replace("[\"search\"]", "[\"project\", \"releases\"]");
        assert!(matches!(
            validate(&releases),
            Err(ProgramError::MissingCapabilityRequirement { .. })
        ));
    }

    #[test]
    fn mappings_accept_only_rfc_6901_pointer_syntax() {
        for pointer in ["/a~2b", "/trailing~", "relative"] {
            let invalid = CATALOG.replace("/hits", pointer);
            assert!(matches!(
                validate(&invalid),
                Err(ProgramError::InvalidPointer(_))
            ));
        }
        let escaped = CATALOG.replace("/hits", "/a~0b~1c");
        assert!(validate(&escaped).is_ok());
    }

    #[test]
    fn the_complete_vocabulary_validates_and_refuses_what_it_cannot_serve() {
        validate(COMPLETE).unwrap();
        for (from, to, expected) in [
            ("versions:{game_version}", "sides:{side}", "template"),
            ("categories:{loader}", "{loader}:{game_version}", "template"),
            (
                "target = \"loaders\" }",
                "target = \"loaders\", literal = \"x\" }",
                "parameter",
            ),
            ("name = \"stable\"", "name = \"loaders\"", "parameter"),
            ("/version/{release}", "/version/{project}", "route"),
            ("maximum = 100", "maximum = 0", "maximum"),
            ("beta = \"beta\"", "beta = \"stable\"", "channel names"),
            ("channel = {", "unmapped_channel = {", "unknown field"),
        ] {
            let broken = COMPLETE.replace(from, to);
            let refused = toml::from_str::<ProviderProgram>(&broken)
                .map_err(|error| error.to_string())
                .and_then(|program| program.validate().map_err(|error| error.to_string()));
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.contains(expected)),
                "{to}: {refused:?}"
            );
        }
        let without_channel = COMPLETE.replace(
            "channel = { pointer = \"/channel\", release = \"stable\", beta = \"beta\", alpha = \"alpha\" }\n",
            "",
        );
        assert!(matches!(
            validate(&without_channel),
            Err(ProgramError::MissingCapabilityRequirement { requirement, .. })
                if requirement == "mapping release.channel"
        ));
        let unused = COMPLETE.replace(", \"updates\"]", "]");
        assert!(matches!(
            validate(&unused),
            Err(ProgramError::UnusedSection("updates"))
        ));
    }
}
