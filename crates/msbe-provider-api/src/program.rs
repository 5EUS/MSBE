//! Signed declarative provider programs interpreted by reviewed runtimes.
//!
//! A program composes closed vocabulary: it selects a runtime MSBE reviewed, and fills that
//! runtime's slots with fixed routes, named request parameters, translation tables, and JSON
//! pointers into responses. It never supplies code, expressions, or arbitrary requests. How target
//! facts are encoded, how releases are filtered and ordered, and which release may replace an
//! installed one belong to the runtime. See `docs/06-providers-and-policy.md` §6.4.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    EnvelopeError, ExtensionEnvelope, ExtensionProvide, Provider, VerifyingKey,
    manifest::{Acquisition, validate_https_url},
};

/// The only provider-program envelope schema understood by this release.
pub const PROGRAM_SCHEMA_VERSION: u32 = 1;

/// The longest route, parameter name, template, or literal a program may declare.
const TEXT_LIMIT: usize = 256;
/// The longest game, edition or other identifier a program may declare.
const IDENTIFIER_LIMIT: usize = 128;

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
    /// The games a catalog serves: each MSBE plan id it serves, and the catalog's identifier for
    /// that game. A catalog refuses a target for any other game.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub games: BTreeMap<String, GameId>,
    /// The catalog's spellings of target values, by fact.
    #[serde(default, skip_serializing_if = "Translations::is_empty")]
    pub translate: Translations,
    /// Fixed endpoint-relative routes for catalog runtimes.
    #[serde(default)]
    pub routes: Routes,
    /// Web pages that send the user to a file MSBE may not download itself.
    #[serde(default, skip_serializing_if = "Pages::is_empty")]
    pub pages: Pages,
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
                    && self.games.is_empty()
                    && self.translate.is_empty()
                    && self.routes.is_empty()
                    && self.pages.is_empty()
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
                self.validate_games()?;
                self.translate.validate()?;
                self.routes.validate()?;
                self.pages.validate()?;
                self.search.validate()?;
                self.releases.validate()?;
                if let Some(updates) = &self.updates {
                    updates.validate()?;
                }
                self.mappings.validate()?;
                self.validate_capabilities()?;
                self.validate_acquisition()
            }
        }
    }

    /// The catalog's identifier for `game` in `edition`, when the program serves it.
    pub fn game_id(&self, game: &str, edition: Option<&str>) -> Option<&str> {
        self.games.get(game)?.for_edition(edition)
    }

    fn validate_games(&self) -> Result<(), ProgramError> {
        if self.games.is_empty() {
            return Err(ProgramError::NoGames);
        }
        for (game, id) in &self.games {
            let invalid = || ProgramError::InvalidGame(game.clone());
            if !is_identifier(game) {
                return Err(invalid());
            }
            match id {
                GameId::Id(id) if is_identifier(id) => {}
                GameId::ByEdition(ids)
                    if !ids.editions.is_empty()
                        && ids.id.as_deref().is_none_or(is_identifier)
                        && ids
                            .editions
                            .iter()
                            .all(|(edition, id)| is_identifier(edition) && is_identifier(id)) => {}
                _ => return Err(invalid()),
            }
        }
        Ok(())
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
        match (has(Capability::Updates), &self.updates) {
            (true, None) => Err(ProgramError::MissingCapabilityRequirement {
                capability: Capability::Updates,
                requirement: "section updates",
            }),
            (false, Some(_)) => Err(ProgramError::UnusedSection("updates")),
            (true, Some(UpdateProtocol::HashLookupV1 { .. })) => {
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
            (true, Some(UpdateProtocol::ReleasesV1 {})) => {
                if !releases {
                    return Err(ProgramError::CapabilityDependency {
                        capability: Capability::Updates,
                        dependency: Capability::Releases,
                    });
                }
                if self.releases.order == ReleaseOrder::Listed {
                    return Err(ProgramError::UnorderedReleases);
                }
                Ok(())
            }
            (false, None) => Ok(()),
        }
    }

    /// Checks that every file the program can describe can also be obtained: a direct catalog maps
    /// download URLs, and a file MSBE may not download has a page to send the user to.
    fn validate_acquisition(&self) -> Result<(), ProgramError> {
        let file = &self.mappings.release.file;
        let has_page = !self.pages.is_empty();
        if file.distributable.is_some() {
            if !self.provider.policy.respects_distribution_flag {
                return Err(ProgramError::DistributionFlagIgnored);
            }
            if !has_page {
                return Err(ProgramError::MissingPage);
            }
        }
        let describes_files = self
            .capabilities
            .iter()
            .any(|capability| matches!(capability, Capability::Releases | Capability::Updates));
        if !describes_files {
            return Ok(());
        }
        match self.provider.acquisition {
            Acquisition::DirectHttps {} if file.url.is_none() => {
                Err(ProgramError::MissingCapabilityRequirement {
                    capability: Capability::Releases,
                    requirement: "mapping release.file.url",
                })
            }
            Acquisition::UserAction {} | Acquisition::BrowserAssisted { .. } if !has_page => {
                Err(ProgramError::MissingPage)
            }
            _ => Ok(()),
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
        let dependency = &release.dependency;
        let textual = dependency.text.is_some();
        for (present, requirement) in [
            (release.id.is_some(), "mapping release.id"),
            (release.number.is_some(), "mapping release.number"),
            (release.published.is_some(), "mapping release.published"),
            (release.files.is_some(), "mapping release.files"),
            (
                release.dependencies.is_some(),
                "mapping release.dependencies",
            ),
            (release.file.name.is_some(), "mapping release.file.name"),
            (
                textual || dependency.project.is_some(),
                "mapping release.dependency.project or release.dependency.text",
            ),
            (
                textual || dependency.kind.is_some(),
                "mapping release.dependency.kind or release.dependency.text",
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

/// A catalog's identifier for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GameId {
    /// One identifier, whatever the edition.
    Id(String),
    /// Identifiers that differ by edition, as when a catalog lists a remaster as its own game.
    ByEdition(EditionGameIds),
}

impl GameId {
    /// The identifier for `edition`: its own, else the default, if the program declares one.
    pub fn for_edition(&self, edition: Option<&str>) -> Option<&str> {
        match self {
            Self::Id(id) => Some(id),
            Self::ByEdition(ids) => edition
                .and_then(|edition| ids.editions.get(edition))
                .or(ids.id.as_ref())
                .map(String::as_str),
        }
    }
}

/// A catalog's identifiers for one game's editions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditionGameIds {
    /// The identifier for an edition not listed, or for an instance that names none. Without one,
    /// such a target is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Identifiers by the plan's edition id.
    pub editions: BTreeMap<String, String>,
}

/// A catalog's spellings of target values, keyed by MSBE's spelling.
///
/// A fact with no table is sent and matched as MSBE spells it. A fact with a table is known to the
/// catalog only by the values it lists: another value is never sent, and matches no release.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Translations {
    /// Game versions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub game_version: BTreeMap<String, String>,
    /// Loaders, including the APIs a loader provides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub loader: BTreeMap<String, String>,
    /// Editions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub edition: BTreeMap<String, String>,
    /// Storefronts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storefront: BTreeMap<String, String>,
}

impl Translations {
    /// Whether no fact is translated.
    pub fn is_empty(&self) -> bool {
        self.game_version.is_empty()
            && self.loader.is_empty()
            && self.edition.is_empty()
            && self.storefront.is_empty()
    }

    /// The table for `fact`. The game has none: [`ProviderProgram::games`] maps it.
    pub const fn table(&self, fact: TargetFact) -> Option<&BTreeMap<String, String>> {
        match fact {
            TargetFact::Game => None,
            TargetFact::GameVersion => Some(&self.game_version),
            TargetFact::Loaders => Some(&self.loader),
            TargetFact::Edition => Some(&self.edition),
            TargetFact::Storefront => Some(&self.storefront),
        }
    }

    fn validate(&self) -> Result<(), ProgramError> {
        [
            &self.game_version,
            &self.loader,
            &self.edition,
            &self.storefront,
        ]
        .into_iter()
        .try_for_each(check_spellings)
    }
}

/// Fixed, endpoint-relative catalog routes.
///
/// Every route may also use `{game}` once, which is filled with the catalog's identifier for the
/// target's game.
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
    /// How project references and ids split into route segments, when a catalog addresses a
    /// project by more than one segment, such as an owner and a name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<SegmentedReference>,
}

impl Routes {
    const fn is_empty(&self) -> bool {
        self.search.is_none()
            && self.project.is_none()
            && self.releases.is_none()
            && self.release.is_none()
            && self.reference.is_none()
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
        self.reference
            .as_ref()
            .map_or(Ok(()), SegmentedReference::validate)
    }
}

/// A project reference made of a fixed number of segments, such as `owner-name`.
///
/// A reference or project id must split on `separator` into exactly `segments` non-empty parts,
/// each safe as a route segment. In routes and pages, `{reference}` and `{project}` are filled with
/// the parts joined by `/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentedReference {
    /// The one character between parts: `-`, `.`, `_`, `+` or `/`.
    pub separator: String,
    /// How many parts, from 2 to 4.
    pub segments: u8,
}

impl SegmentedReference {
    fn validate(&self) -> Result<(), ProgramError> {
        if matches!(self.separator.as_str(), "-" | "." | "_" | "+" | "/")
            && (2..=4).contains(&self.segments)
        {
            Ok(())
        } else {
            Err(ProgramError::InvalidSeparator(self.separator.clone()))
        }
    }
}

/// Absolute HTTPS pages for files MSBE may not download itself.
///
/// A page may use `{game}` once. The project page must use `{project}` once; the release page must
/// use `{release}` once and may use `{project}` once. The release page is preferred.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pages {
    /// A project's page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The page listing one release's files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

impl Pages {
    /// Whether the program declares no page.
    pub const fn is_empty(&self) -> bool {
        self.project.is_none() && self.release.is_none()
    }

    fn validate(&self) -> Result<(), ProgramError> {
        if let Some(page) = &self.project {
            check_page(page, &["project"], &[])?;
        }
        if let Some(page) = &self.release {
            check_page(page, &["release"], &["project"])?;
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
    /// Query parameters sent with every search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<QueryParameter>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: default_query(),
            limit: default_limit(),
            maximum: None,
            facets: None,
            parameters: Vec::new(),
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
        check_parameters(&self.parameters)?;
        names.extend(
            self.parameters
                .iter()
                .map(|parameter| parameter.name.as_str()),
        );
        check_names(&names)
    }
}

/// Grouped filters a search sends as one query parameter: a JSON array of arrays of strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facets {
    /// The query parameter carrying the groups.
    pub parameter: String,
    /// Groups of templates. A template may hold one placeholder: `{game}`, `{game_version}`,
    /// `{edition}`, `{storefront}`, or `{loader}`, which repeats the template once for every loader
    /// the target accepts. Values are the catalog's spellings. A group that expands to nothing is
    /// left out.
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
        check_parameters(&self.query)?;
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
    /// A target fact. A target without the fact, or whose values the catalog has no spelling for,
    /// does not send the parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetFact>,
    /// Spellings for this parameter alone, keyed by MSBE's spelling, such as the numbers a catalog
    /// identifies loaders by in requests. They replace the program's translation of the fact; a
    /// value they do not list is not sent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
    /// How the fact's values are written.
    #[serde(default, skip_serializing_if = "Encoding::is_default")]
    pub encoding: Encoding,
}

/// How a query parameter writes a target fact's values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encoding {
    /// One parameter holding a JSON array of strings.
    #[default]
    JsonArray,
    /// One parameter holding the values separated by commas.
    Comma,
    /// The parameter repeated once per value.
    Repeated,
    /// One parameter holding one value; with any other number of values it is not sent, and the
    /// runtime filters the answer instead.
    Single,
}

impl Encoding {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes the field by reference"
    )]
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A fact about the target a request may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetFact {
    /// The catalog's identifier for the target's game.
    Game,
    /// Every loader the target accepts: its own and those it provides.
    Loaders,
    /// The target's game version.
    GameVersion,
    /// The target's edition.
    Edition,
    /// The target's storefront.
    Storefront,
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
    /// Highest semantic version first, ignoring a leading `v`; releases whose number is not a
    /// semantic version follow, newest publication first.
    Semver,
}

/// How a catalog is asked for updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum UpdateProtocol {
    /// Bulk lookups by file hash, one for the installed releases and one per release channel in
    /// use for the newest compatible release. A file stays on its channel or moves to a more stable
    /// one, and moves to an older release only to regain compatibility with the target.
    HashLookupV1 {
        /// The published file hash installed files are looked up by.
        algorithm: HashAlgorithm,
        /// Route answering which release each hashed file belongs to.
        listed: String,
        /// Route answering the newest release, admitted by filters, of each hashed file's project.
        latest: String,
        /// The request body's field names.
        fields: UpdateFields,
    },
    /// Lists each installed project's releases, in the program's release order, and compares them
    /// with the installed one, under the same rules as `hash-lookup-v1`. For catalogs with no bulk
    /// or hash lookup, at one request per project.
    ReleasesV1 {},
}

impl UpdateProtocol {
    /// The hash installed files are looked up by, when the protocol looks files up by hash.
    pub const fn algorithm(&self) -> Option<HashAlgorithm> {
        match self {
            Self::HashLookupV1 { algorithm, .. } => Some(*algorithm),
            Self::ReleasesV1 {} => None,
        }
    }

    fn validate(&self) -> Result<(), ProgramError> {
        match self {
            Self::HashLookupV1 {
                listed,
                latest,
                fields,
                ..
            } => {
                check_route(listed, None)?;
                check_route(latest, None)?;
                check_names(&[
                    &fields.hashes,
                    &fields.algorithm,
                    &fields.loaders,
                    &fields.game_versions,
                    &fields.channels,
                ])
            }
            Self::ReleasesV1 {} => Ok(()),
        }
    }
}

/// A published file hash algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    /// MD5, which some catalogs still publish. Detects corruption, not tampering.
    Md5,
    /// SHA-1. Detects corruption, not tampering.
    Sha1,
    /// SHA-256.
    Sha256,
    /// SHA-512.
    Sha512,
}

impl HashAlgorithm {
    /// The algorithm's name, as provenance records and catalogs spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
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
    /// The game versions a replacement must support. Left out when the target has none.
    pub game_versions: String,
    /// The release channels a replacement may be on.
    pub channels: String,
}

/// Bounded JSON-pointer mappings used by `catalog-v1`.
///
/// Where a catalog writes a number, a field read as text takes its decimal digits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mappings {
    /// Pointer to the array of search objects.
    pub search_items: Option<String>,
    /// The releases in a release listing. Unmapped, the listing is an array of releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releases: Option<Items>,
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
    fn is_empty(&self) -> bool {
        self.search_items.is_none()
            && self.releases.is_none()
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
            check_distinct_names(&[&channel.release, &channel.beta, &channel.alpha])
                .map_err(|()| ProgramError::InvalidChannels)?;
        }
        self.release.dependency.validate()?;
        if let Some(extension) = &self.release.file.extension
            && (extension.is_empty()
                || extension.len() > 16
                || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        {
            return Err(ProgramError::InvalidExtension(extension.clone()));
        }
        Ok(())
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        self.search_items
            .iter()
            .chain(self.releases.iter().map(Items::pointer))
            .chain(self.hit.iter().flat_map(ObjectMapping::all_pointers))
            .chain(self.project.all_pointers())
            .chain(self.release.all_pointers())
    }
}

/// Items at a pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Items {
    /// A pointer to an array of items.
    Array(String),
    /// A pointer to one object, taken as the only item, such as a release that is its own file.
    Single(SingleItem),
}

impl Items {
    /// The pointer.
    pub const fn pointer(&self) -> &String {
        match self {
            Self::Array(pointer) | Self::Single(SingleItem { single: pointer }) => pointer,
        }
    }
}

/// A pointer to one object taken as a list of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleItem {
    /// The pointer; `""` is the enclosing object itself.
    pub single: String,
}

/// Where a field's values are in a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Selector {
    /// A pointer to one value, or to an array of values.
    Pointer(String),
    /// A value inside each object of an array, such as the hash in each `{ algo, value }` entry.
    Each(EachSelector),
}

impl Selector {
    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        let (first, second, third) = match self {
            Self::Pointer(pointer) => (pointer, None, None),
            Self::Each(each) => (
                &each.each,
                Some(&each.value),
                each.when.as_ref().map(|condition| &condition.pointer),
            ),
        };
        std::iter::once(first).chain(second).chain(third)
    }
}

/// A value inside each object of an array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EachSelector {
    /// Pointer to the array of objects.
    pub each: String,
    /// Pointer to the value within each object.
    pub value: String,
    /// Keeps only objects that meet a condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Condition>,
}

/// An object's value at `pointer` is `equals`, compared as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// Pointer within the object.
    pub pointer: String,
    /// The text the value must be.
    pub equals: String,
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
    /// The catalog game identifiers a project is listed for. Mapped, a project not listed for the
    /// target's game is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub games: Option<Selector>,
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
            && self.games.is_none()
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
        .chain(self.games.iter().flat_map(Selector::all_pointers))
    }
}

/// Release fields selected from a JSON object.
///
/// A compatibility list that is unmapped does not constrain a release. Mapped, a release that lists
/// no loaders supports none; a release that lists no game versions, editions or storefronts
/// supports any; and a target without the fact accepts any release.
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
    /// The release's downloadable files.
    pub files: Option<Items>,
    /// Supported game versions.
    pub game_versions: Option<Selector>,
    /// Supported loaders.
    pub loaders: Option<Selector>,
    /// Supported editions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editions: Option<Selector>,
    /// Supported storefronts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storefronts: Option<Selector>,
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
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.project.is_none()
            && self.number.is_none()
            && self.published.is_none()
            && self.files.is_none()
            && self.game_versions.is_none()
            && self.loaders.is_none()
            && self.editions.is_none()
            && self.storefronts.is_none()
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
            &self.loader_versions,
            &self.dependencies,
        ]
        .into_iter()
        .flatten()
        .chain(self.files.iter().map(Items::pointer))
        .chain(
            [
                &self.game_versions,
                &self.loaders,
                &self.editions,
                &self.storefronts,
            ]
            .into_iter()
            .flatten()
            .flat_map(Selector::all_pointers),
        )
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
    /// Download URL pointer. A file whose URL is absent or null is downloaded by the user from its
    /// page.
    pub url: Option<String>,
    /// File name pointer.
    pub name: Option<String>,
    /// File size pointer.
    pub size: Option<String>,
    /// MD5 digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<Selector>,
    /// SHA-1 digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha1: Option<Selector>,
    /// SHA-256 digest.
    pub sha256: Option<Selector>,
    /// SHA-512 digest.
    pub sha512: Option<Selector>,
    /// Primary-file flag pointer. Unmapped, every file is primary; mapped, a file without the flag
    /// is not.
    pub primary: Option<String>,
    /// Pointer to the author's flag allowing third-party downloads. A file whose flag is `false`,
    /// or not a boolean, is downloaded by the user from its page; an absent flag allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distributable: Option<String>,
    /// The extension a file name is given when the catalog's name lacks it, such as `zip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

impl FileMapping {
    const fn is_empty(&self) -> bool {
        self.url.is_none()
            && self.name.is_none()
            && self.size.is_none()
            && self.md5.is_none()
            && self.sha1.is_none()
            && self.sha256.is_none()
            && self.sha512.is_none()
            && self.primary.is_none()
            && self.distributable.is_none()
            && self.extension.is_none()
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        [
            &self.url,
            &self.name,
            &self.size,
            &self.primary,
            &self.distributable,
        ]
        .into_iter()
        .flatten()
        .chain(
            [&self.md5, &self.sha1, &self.sha256, &self.sha512]
                .into_iter()
                .flatten()
                .flat_map(Selector::all_pointers),
        )
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
    /// Relationship-kind pointer. Its values are `required`, `optional`, `incompatible` and
    /// `embedded`, or the catalog's names for them in `kinds`.
    pub kind: Option<String>,
    /// The catalog's names for relationship kinds. A kind it does not name is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<DependencyKinds>,
    /// Dependencies written as text rather than objects, in place of the pointers above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextDependency>,
}

impl DependencyMapping {
    const fn is_empty(&self) -> bool {
        self.project.is_none()
            && self.release.is_none()
            && self.kind.is_none()
            && self.kinds.is_none()
            && self.text.is_none()
    }

    fn all_pointers(&self) -> impl Iterator<Item = &String> {
        [&self.project, &self.release, &self.kind]
            .into_iter()
            .flatten()
    }

    fn validate(&self) -> Result<(), ProgramError> {
        if let Some(text) = &self.text {
            let single_punctuation =
                matches!(text.separator.as_bytes(), [byte] if byte.is_ascii_punctuation());
            if !single_punctuation {
                return Err(ProgramError::InvalidSeparator(text.separator.clone()));
            }
            if self.project.is_some()
                || self.release.is_some()
                || self.kind.is_some()
                || self.kinds.is_some()
            {
                return Err(ProgramError::InvalidDependencyMapping);
            }
        }
        if let Some(kinds) = &self.kinds {
            let names: Vec<&String> = [
                &kinds.required,
                &kinds.optional,
                &kinds.incompatible,
                &kinds.embedded,
            ]
            .into_iter()
            .flatten()
            .collect();
            if self.kind.is_none() || names.is_empty() || check_distinct_names(&names).is_err() {
                return Err(ProgramError::InvalidDependencyMapping);
            }
        }
        Ok(())
    }
}

/// The catalog's names for relationship kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyKinds {
    /// A project that must be installed too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<String>,
    /// A project that adds functionality if installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<String>,
    /// A project that must not be installed alongside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incompatible: Option<String>,
    /// A project bundled inside the release's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded: Option<String>,
}

/// Required dependencies written as `<project><separator><version>` text.
///
/// The project is everything before the last separator. The version is the minimum the author
/// built against; MSBE resolves the project's releases rather than pinning it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextDependency {
    /// The one ASCII punctuation character before the version.
    pub separator: String,
}

/// Refuses a route that can escape its origin, or whose placeholders are not exactly
/// `placeholder`, once, and `{game}` at most once.
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
    let rest = rest.replacen("{game}", "", 1);
    if rest.contains(['{', '}', '?', '#', ' ']) {
        return Err(invalid());
    }
    Ok(())
}

/// Refuses a page that is not an absolute HTTPS URL, puts a placeholder in its host, or uses
/// placeholders other than `required` once each, and `optional` and `{game}` at most once each.
fn check_page(page: &str, required: &[&str], optional: &[&str]) -> Result<(), ProgramError> {
    let invalid = || ProgramError::InvalidPage(page.to_owned());
    if validate_https_url(page).is_err()
        || !page.is_ascii()
        || page.len() > TEXT_LIMIT
        || page.contains(char::is_whitespace)
        || page.contains("..")
    {
        return Err(invalid());
    }
    let host = page
        .trim_start_matches("https://")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if host.contains(['{', '}']) {
        return Err(invalid());
    }
    let mut rest = page.to_owned();
    for (name, needed) in required
        .iter()
        .map(|name| (*name, true))
        .chain(optional.iter().map(|name| (*name, false)))
        .chain(std::iter::once(("game", false)))
    {
        let marker = format!("{{{name}}}");
        match rest.matches(&marker).count() {
            1 => rest = rest.replacen(&marker, "", 1),
            0 if !needed => {}
            _ => return Err(invalid()),
        }
    }
    if rest.contains(['{', '}']) {
        return Err(invalid());
    }
    Ok(())
}

/// Refuses a facet template with an unknown placeholder, or more than one.
fn check_template(template: &str) -> Result<(), ProgramError> {
    const PLACEHOLDERS: [&str; 5] = [
        "{game}",
        "{game_version}",
        "{loader}",
        "{edition}",
        "{storefront}",
    ];
    let markers: usize = PLACEHOLDERS
        .iter()
        .map(|marker| template.matches(marker).count())
        .sum();
    let rest = PLACEHOLDERS
        .iter()
        .fold(template.to_owned(), |rest, marker| {
            rest.replacen(marker, "", 1)
        });
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

/// Refuses parameters that have no single value, or whose spellings or encoding do not fit it.
fn check_parameters(parameters: &[QueryParameter]) -> Result<(), ProgramError> {
    for parameter in parameters {
        let valid = match (&parameter.literal, parameter.target) {
            (Some(literal), None) => {
                literal.is_ascii()
                    && literal.len() <= TEXT_LIMIT
                    && parameter.values.is_empty()
                    && parameter.encoding.is_default()
            }
            (None, Some(_)) => check_spellings(&parameter.values).is_ok(),
            _ => false,
        };
        if !valid {
            return Err(ProgramError::InvalidParameter(parameter.name.clone()));
        }
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

/// Refuses a translation table whose keys are not identifiers or whose spellings are empty, not
/// printable ASCII, or too long.
fn check_spellings(table: &BTreeMap<String, String>) -> Result<(), ProgramError> {
    match table.iter().find(|(key, spelling)| {
        !is_identifier(key)
            || spelling.is_empty()
            || spelling.len() > TEXT_LIMIT
            || !spelling
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    }) {
        Some((key, _)) => Err(ProgramError::InvalidTranslation(key.clone())),
        None => Ok(()),
    }
}

/// Whether names are non-empty, ASCII and pairwise distinct.
fn check_distinct_names(names: &[&String]) -> Result<(), ()> {
    let distinct = names
        .iter()
        .enumerate()
        .all(|(index, name)| !names.iter().take(index).any(|earlier| earlier == name));
    if distinct && names.iter().all(|name| !name.is_empty() && name.is_ascii()) {
        Ok(())
    } else {
        Err(())
    }
}

/// Whether `raw` is safe as an identifier and as one route segment.
fn is_identifier(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= IDENTIFIER_LIMIT
        && raw.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
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
    #[error(
        "{0} does not accept games, translations, routes, pages, requests, mappings, or capabilities"
    )]
    UnexpectedConfiguration(&'static str),
    /// A section is declared without the capability that uses it.
    #[error("provider program declares [{0}] without the capability that uses it")]
    UnusedSection(&'static str),
    /// A catalog runtime has no API origin.
    #[error("catalog program {0:?} has no metadata endpoint")]
    MissingMetadata(String),
    /// A catalog does not say which games it serves.
    #[error("a catalog program must map at least one game in [games]")]
    NoGames,
    /// A game or its catalog identifier is not a safe identifier.
    #[error("invalid provider program game {0:?}")]
    InvalidGame(String),
    /// A translation's key is not an identifier, or its spelling is empty or unprintable.
    #[error("invalid provider program translation for {0:?}")]
    InvalidTranslation(String),
    /// A route can escape its API origin.
    #[error("invalid provider program route {0:?}")]
    InvalidRoute(String),
    /// A page is not an HTTPS URL with the placeholders it takes.
    #[error("invalid provider program page {0:?}")]
    InvalidPage(String),
    /// A pointer is outside the supported subset.
    #[error("invalid provider program JSON pointer {0:?}")]
    InvalidPointer(String),
    /// A request parameter or field name is malformed, repeated, or has no single value.
    #[error("invalid provider program request parameter {0:?}")]
    InvalidParameter(String),
    /// A facet template has an unknown placeholder, or more than one.
    #[error("invalid provider program template {0:?}")]
    InvalidTemplate(String),
    /// A reference or dependency separator is not one allowed character, or a reference has too
    /// few or too many segments.
    #[error("invalid provider program separator or segment count {0:?}")]
    InvalidSeparator(String),
    /// A file-name extension is empty, too long or not alphanumeric.
    #[error("invalid provider program file extension {0:?}")]
    InvalidExtension(String),
    /// Text dependencies mixed with dependency pointers, or relationship names that are not
    /// distinct or have no kind pointer.
    #[error(
        "provider program dependency mapping must use either pointers with distinct kind names, or text"
    )]
    InvalidDependencyMapping,
    /// A search maximum of zero.
    #[error("a provider program search maximum must be at least 1")]
    InvalidLimit,
    /// Channel names that are empty or not distinct.
    #[error("provider program channel names must be distinct and non-empty")]
    InvalidChannels,
    /// A distribution flag is mapped by a provider whose policy does not respect it.
    #[error("provider program maps a distribution flag, but its policy does not respect one")]
    DistributionFlagIgnored,
    /// A file MSBE may not download has no page to send the user to.
    #[error("provider program needs [pages] for files MSBE may not download itself")]
    MissingPage,
    /// `releases-v1` cannot tell a newer release from an older one in the catalog's order.
    #[error("the releases-v1 update protocol needs [releases] order newest-first or semver")]
    UnorderedReleases,
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
        [games]
        game = "game"
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
        [games]
        game = "game"
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

    /// A catalog serving several games, with numeric identifiers, per-file distribution flags and
    /// weak digests, and no hash lookup.
    const MULTI_GAME: &str = r#"
        runtime = "catalog-v1"
        capabilities = ["search", "project", "releases", "updates"]
        [games]
        first = "432"
        second = { id = "1704", editions = { original = "73" } }
        [translate]
        loader = { alpha = "Alpha", beta = "Beta" }
        storefront = { store = "Store" }
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
        respects_distribution_flag = true
        tos_url = ""
        ack_required = false
        [routes]
        search = "/games/{game}/search"
        project = "/mods/{reference}"
        releases = "/mods/{project}/files"
        [pages]
        release = "https://www.example.test/{game}/mods/{project}?file={release}"
        [search]
        query = "searchFilter"
        parameters = [
          { name = "gameId", target = "game", encoding = "single" },
          { name = "modLoaderType", target = "loaders", encoding = "single", values = { alpha = "1", beta = "4" } },
          { name = "gameVersion", target = "game-version", encoding = "single" },
        ]
        [releases]
        order = "semver"
        query = [{ name = "storefronts", target = "storefront", encoding = "comma" }, { name = "tags", target = "loaders", encoding = "repeated" }]
        [updates]
        type = "releases-v1"
        [mappings]
        search_items = "/data"
        releases = "/data"
        [mappings.project]
        id = "/data/id"
        title = "/data/name"
        games = { each = "/data/games", value = "/id" }
        [mappings.release]
        id = "/id"
        number = "/displayName"
        published = "/fileDate"
        files = { single = "" }
        game_versions = "/gameVersions"
        loaders = "/gameVersions"
        storefronts = "/stores"
        dependencies = "/dependencies"
        [mappings.release.file]
        url = "/downloadUrl"
        name = "/fileName"
        size = "/fileLength"
        sha1 = { each = "/hashes", value = "/value", when = { pointer = "/algo", equals = "1" } }
        md5 = { each = "/hashes", value = "/value", when = { pointer = "/algo", equals = "2" } }
        distributable = "/isAvailable"
        extension = "zip"
        [mappings.release.dependency]
        project = "/modId"
        kind = "/relationType"
        kinds = { required = "3", optional = "2", incompatible = "5", embedded = "1" }
    "#;

    fn validate(document: &str) -> Result<(), ProgramError> {
        toml::from_str::<ProviderProgram>(document)
            .unwrap()
            .validate()
    }

    /// The error `document` is refused with, as text, whether it fails to parse or to validate.
    fn refusal(document: &str) -> Option<String> {
        toml::from_str::<ProviderProgram>(document)
            .map_err(|error| error.to_string())
            .and_then(|program| program.validate().map_err(|error| error.to_string()))
            .err()
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
            let refused = refusal(&broken);
            assert!(
                refused
                    .as_ref()
                    .is_some_and(|error| error.contains(expected)),
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

    #[test]
    fn a_catalog_names_the_games_it_serves_and_their_catalog_identifiers() {
        assert!(matches!(
            validate(&CATALOG.replace("[games]\n        game = \"game\"\n", "")),
            Err(ProgramError::NoGames)
        ));
        for games in [
            "game = \"../escape\"",
            "game = \"a/b\"",
            "\"\" = \"game\"",
            "game = { editions = {} }",
            "game = { id = \"?\", editions = { remaster = \"r\" } }",
        ] {
            let broken = CATALOG.replace("game = \"game\"", games);
            assert!(
                matches!(validate(&broken), Err(ProgramError::InvalidGame(_))),
                "{games}"
            );
        }

        let program: ProviderProgram = toml::from_str(MULTI_GAME).unwrap();
        program.validate().unwrap();
        assert_eq!(program.game_id("first", None), Some("432"));
        assert_eq!(program.game_id("second", Some("original")), Some("73"));
        assert_eq!(program.game_id("second", Some("remaster")), Some("1704"));
        assert_eq!(program.game_id("third", None), None);
        let edition_only = MULTI_GAME.replace("id = \"1704\", ", "");
        let program: ProviderProgram = toml::from_str(&edition_only).unwrap();
        assert_eq!(program.game_id("second", None), None);
    }

    #[test]
    fn translations_encodings_selectors_and_weak_digests_validate() {
        validate(MULTI_GAME).unwrap();
        for (from, to, expected) in [
            ("alpha = \"Alpha\"", "alpha = \"\"", "translation"),
            ("store = \"Store\"", "\"st/ore\" = \"Store\"", "translation"),
            (
                "{ name = \"gameId\", target = \"game\", encoding = \"single\" }",
                "{ name = \"gameId\", literal = \"432\", encoding = \"single\" }",
                "parameter",
            ),
            (
                "encoding = \"comma\"",
                "encoding = \"csv\"",
                "unknown variant",
            ),
            ("/games/{game}/search", "/games/{game}/{game}", "route"),
            (
                "equals = \"1\" }",
                "equals = \"1\", extra = true }",
                "did not match",
            ),
            ("extension = \"zip\"", "extension = \".zip\"", "extension"),
            (
                "kinds = { required = \"3\", optional = \"2\"",
                "kinds = { required = \"3\", optional = \"3\"",
                "dependency mapping",
            ),
        ] {
            let refused = refusal(&MULTI_GAME.replace(from, to));
            assert!(
                refused
                    .as_ref()
                    .is_some_and(|error| error.contains(expected)),
                "{to}: {refused:?}"
            );
        }
    }

    #[test]
    fn files_msbe_may_not_download_need_a_page_and_a_respected_flag() {
        let ignored = MULTI_GAME.replace(
            "respects_distribution_flag = true",
            "respects_distribution_flag = false",
        );
        assert!(matches!(
            validate(&ignored),
            Err(ProgramError::DistributionFlagIgnored)
        ));
        let pageless = MULTI_GAME.replace(
            "[pages]\n        release = \"https://www.example.test/{game}/mods/{project}?file={release}\"\n",
            "",
        );
        assert!(matches!(
            validate(&pageless),
            Err(ProgramError::MissingPage)
        ));
        let website_only = pageless
            .replace("distributable = \"/isAvailable\"\n", "")
            .replace("type = \"direct_https\"", "type = \"user_action\"")
            .replace("url = \"/downloadUrl\"\n", "");
        assert!(matches!(
            validate(&website_only),
            Err(ProgramError::MissingPage)
        ));
        let with_page = website_only.replace(
            "[search]",
            "[pages]\n        project = \"https://www.example.test/mods/{project}\"\n        [search]",
        );
        validate(&with_page).unwrap();
        let direct_without_url =
            with_page.replace("type = \"user_action\"", "type = \"direct_https\"");
        assert!(matches!(
            validate(&direct_without_url),
            Err(ProgramError::MissingCapabilityRequirement { requirement, .. })
                if requirement == "mapping release.file.url"
        ));

        for page in [
            "http://www.example.test/{game}/mods/{project}?file={release}",
            "https://{game}.example.test/mods/{project}?file={release}",
            "https://www.example.test/{game}/mods/{project}",
            "https://www.example.test/{loader}/mods/{project}?file={release}",
        ] {
            let broken = MULTI_GAME.replace(
                "https://www.example.test/{game}/mods/{project}?file={release}",
                page,
            );
            assert!(
                matches!(validate(&broken), Err(ProgramError::InvalidPage(_))),
                "{page}"
            );
        }
    }

    #[test]
    fn releases_v1_needs_an_order_and_accepts_no_hash_lookup_fields() {
        assert!(matches!(
            validate(&MULTI_GAME.replace("order = \"semver\"", "order = \"listed\"")),
            Err(ProgramError::UnorderedReleases)
        ));
        let with_lookup_fields = MULTI_GAME.replace(
            "type = \"releases-v1\"",
            "type = \"releases-v1\"\n        algorithm = \"sha1\"",
        );
        assert!(refusal(&with_lookup_fields).is_some_and(|error| error.contains("algorithm")));
        let lookup_without_fields =
            MULTI_GAME.replace("type = \"releases-v1\"", "type = \"hash-lookup-v1\"");
        assert!(
            refusal(&lookup_without_fields).is_some_and(|error| error.contains("missing field"))
        );
    }

    #[test]
    fn segmented_references_and_text_dependencies_are_bounded() {
        let segmented = COMPLETE.replace(
            "release = \"/version/{release}\"",
            "release = \"/version/{release}\"\n        reference = { separator = \"-\", segments = 2 }",
        );
        validate(&segmented).unwrap();
        for shape in [
            "separator = \"?\", segments = 2",
            "separator = \"-\", segments = 1",
            "separator = \"--\", segments = 2",
        ] {
            let broken = segmented.replace("separator = \"-\", segments = 2", shape);
            assert!(
                matches!(validate(&broken), Err(ProgramError::InvalidSeparator(_))),
                "{shape}"
            );
        }

        let textual = COMPLETE.replace(
            "[mappings.release.dependency]\n        project = \"/project_id\"\n        kind = \"/kind\"",
            "[mappings.release.dependency]\n        text = { separator = \"-\" }",
        );
        let textual = textual
            .replace("\"release-project\", ", "")
            .replace("        release = \"/version/{release}\"\n", "");
        validate(&textual).unwrap();
        let mixed = textual.replace(
            "text = { separator = \"-\" }",
            "text = { separator = \"-\" }\n        kind = \"/kind\"",
        );
        assert!(matches!(
            validate(&mixed),
            Err(ProgramError::InvalidDependencyMapping)
        ));
    }
}
