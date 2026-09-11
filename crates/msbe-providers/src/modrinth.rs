//! The Modrinth provider.
//!
//! Modrinth serves project metadata without authentication, publishes a dependency graph, and
//! hashes every file, which is why it is the first provider (`docs/06-providers-and-policy.md`).
//! Its API requires a uniquely identifying `User-Agent`, which `msbe-http` sets.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt, io,
    path::{Path, PathBuf},
};

use msbe_core::solver::{
    Candidate, PackageId, Requirement as SolverRequirement, RootRequirement, Solution, solve,
};
use msbe_plan_schema::Side;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    Availability, EndpointError, JsonEndpoint, Overlay, Target,
    acquisition::{AcquisitionError, ArtifactDescriptor, acquire},
    http::{HttpClient, HttpError},
};

/// Modrinth's production API.
pub const API_BASE: &str = "https://api.modrinth.com/v2";

/// The most bytes a metadata response may be.
const METADATA_LIMIT: u64 = 16 << 20;

/// The most search results Modrinth returns in one request.
const MAX_SEARCH_LIMIT: u8 = 100;

/// A request for a project, optionally pinned to a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The project's slug or id.
    pub project: String,
    /// A version number or version id to pin, if any.
    pub version: Option<String>,
}

impl Spec {
    /// Parses `project` or `project@version`.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError::InvalidSpec`] if either part is empty or contains a character
    /// that cannot appear in a slug, id or version number.
    pub fn parse(raw: &str) -> Result<Self, ModrinthError> {
        let (project, version) = match raw.split_once('@') {
            Some((project, version)) => (project, Some(version)),
            None => (raw, None),
        };
        if !is_reference(project) || version.is_some_and(|version| !is_reference(version)) {
            return Err(ModrinthError::InvalidSpec(raw.to_owned()));
        }
        Ok(Self {
            project: project.to_owned(),
            version: version.map(str::to_owned),
        })
    }
}

/// A Modrinth project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// The stable id.
    pub id: String,
    /// The current slug, which can change.
    pub slug: String,
    /// The display title.
    pub title: String,
    /// The kind of project, such as `mod`.
    pub project_type: String,
    /// Whether the project supports player clients.
    #[serde(default)]
    pub client_side: Availability,
    /// Whether the project supports dedicated servers.
    #[serde(default)]
    pub server_side: Availability,
}

/// One published version of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// The stable id.
    pub id: String,
    /// The project it belongs to.
    pub project_id: String,
    /// The version number, as its author wrote it.
    pub version_number: String,
    /// How stable it is.
    pub version_type: VersionType,
    /// When it was published, as an RFC 3339 timestamp.
    pub date_published: String,
    /// The loaders it supports.
    #[serde(default)]
    pub loaders: Vec<String>,
    /// Per-loader versions this release supports, when the provider declares them.
    #[serde(default)]
    pub loader_versions: BTreeMap<String, Vec<String>>,
    /// The game versions it supports.
    #[serde(default)]
    pub game_versions: Vec<String>,
    /// Its downloadable files.
    #[serde(default)]
    pub files: Vec<VersionFile>,
    /// Its declared relationships to other projects.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

/// How stable a version is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionType {
    /// A stable release.
    Release,
    /// A pre-release.
    Beta,
    /// An early, unstable build.
    Alpha,
    /// A type this client does not know.
    #[serde(other)]
    Unknown,
}

/// A downloadable file of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionFile {
    /// Its published hashes.
    pub hashes: Hashes,
    /// Where to download it.
    pub url: String,
    /// Its file name.
    pub filename: String,
    /// Whether it is the version's main file.
    #[serde(default)]
    pub primary: bool,
    /// Its size in bytes.
    pub size: u64,
}

/// The hashes Modrinth publishes for a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hashes {
    /// SHA-512 as hex.
    pub sha512: String,
    /// SHA-1 as hex.
    #[serde(default)]
    pub sha1: Option<String>,
}

/// A version's relationship to another project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// A specific version, if pinned.
    #[serde(default)]
    pub version_id: Option<String>,
    /// The related project, if given.
    #[serde(default)]
    pub project_id: Option<String>,
    /// The kind of relationship.
    pub dependency_type: DependencyType,
}

/// The kind of relationship a dependency declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyType {
    /// The project must be installed too.
    Required,
    /// The project adds functionality if installed.
    Optional,
    /// The project must not be installed alongside.
    Incompatible,
    /// The project is bundled inside this file.
    Embedded,
    /// A kind this client does not know.
    #[serde(other)]
    Unknown,
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The project's stable id.
    pub project_id: String,
    /// The project's slug.
    pub slug: String,
    /// The display title.
    pub title: String,
    /// A short description.
    #[serde(default)]
    pub description: String,
    /// Total downloads.
    #[serde(default)]
    pub downloads: u64,
    /// Whether the project supports player clients.
    #[serde(default)]
    pub client_side: Availability,
    /// Whether the project supports dedicated servers.
    #[serde(default)]
    pub server_side: Availability,
}

#[derive(Deserialize)]
struct SearchResults {
    hits: Vec<SearchHit>,
}

/// A version chosen for installation, and the file to install from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selection {
    /// The project.
    pub project: Project,
    /// The chosen version.
    pub version: Version,
    /// The file to install.
    pub file: VersionFile,
    /// The slug of the project whose requirement brought this in, if any.
    pub required_by: Option<String>,
}

/// A declared relationship to a project, and the project that declared it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    /// The project referred to.
    pub project_id: String,
    /// The slug of the project that declared it.
    pub declared_by: String,
}

/// What installing a set of projects involves.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct InstallPlan {
    /// Every project to install, requested ones first.
    pub selections: Vec<Selection>,
    /// Required dependencies that were not selected.
    pub unresolved: Vec<Requirement>,
    /// Declared incompatibilities between selected projects.
    pub incompatible: Vec<Requirement>,
    /// Required projects that another selected or installed project stands in for.
    pub substitutions: Vec<Substitution>,
}

/// A version that should replace an installed one, and the file to install from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Update {
    /// The installed version.
    pub installed: Version,
    /// The version to move to.
    pub version: Version,
    /// The file to install from it.
    pub file: VersionFile,
}

/// What updating one installed file would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// Modrinth does not list the file, so there is nothing to compare it with.
    Unlisted,
    /// The installed version is the newest compatible one on its channel.
    Current(Box<Version>),
    /// The installed version does not support the target, and nothing on its channel does.
    Incompatible(Box<Version>),
    /// Another version should replace the installed one.
    Available(Box<Update>),
}

/// The projects a version requires, and the ones it declares incompatible.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Relationships {
    /// Projects that must be installed too.
    pub required: Vec<Requirement>,
    /// Projects that must not be installed alongside.
    pub incompatible: Vec<Requirement>,
}

/// The body of Modrinth's bulk lookups by file hash.
#[derive(Serialize)]
struct HashQuery<'q> {
    hashes: &'q [&'q str],
    algorithm: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    loaders: Option<&'q [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    game_versions: Option<[&'q str; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_types: Option<&'q [&'q str]>,
}

/// A Modrinth release already in the profile, which resolution keeps as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRelease {
    /// The project id.
    pub project: String,
    /// The version id.
    pub version: String,
}

/// A required project that another selected or installed project provides or replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Substitution {
    /// The required project.
    pub project_id: String,
    /// The slug of the project that requires it.
    pub declared_by: String,
    /// The project that stands in for it.
    pub supplied_by: String,
}

type ReleaseKey = (String, String);
type ReleaseRecord = (Project, Version);

/// Why a project is collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// It was requested or is required, so it must exist.
    Required,
    /// The overlay says it could stand in for a required project.
    StandIn,
}

#[derive(Default)]
struct CandidateGraph {
    /// The requested projects' ids, in request order.
    requested: Vec<String>,
    roots: Vec<RootRequirement>,
    candidates: Vec<Candidate>,
    records: BTreeMap<ReleaseKey, ReleaseRecord>,
    /// Installed releases the walk did not collect, which are candidates without a record.
    kept: BTreeSet<ReleaseKey>,
}

impl CandidateGraph {
    /// Rewrites requested version numbers to the release ids the solver knows them by.
    fn pin_requested_versions(&mut self) {
        for root in &mut self.roots {
            let Some(wanted) = root.release.as_deref() else {
                continue;
            };
            let project = root.package.project.as_str();
            let release = self
                .records
                .get(&(project.to_owned(), wanted.to_owned()))
                .or_else(|| {
                    self.records.values().find(|(candidate, version)| {
                        candidate.id == project && version.version_number == wanted
                    })
                })
                .map(|(_, version)| version.id.clone());
            if let Some(release) = release {
                root.release = Some(release);
            }
        }
    }

    /// Requires every `installed` release exactly. One the walk did not collect, because nothing
    /// reached its project or it no longer suits the target, becomes a candidate without
    /// dependencies: what it needed was settled when it was installed.
    fn keep_installed(&mut self, installed: &[InstalledRelease], overlay: &Overlay) {
        let mut order = 0_u64;
        for release in installed {
            let package = package(&release.project);
            let key = (release.project.clone(), release.version.clone());
            if !self.records.contains_key(&key) && self.kept.insert(key) {
                let supplies = overlay.entry(&package).cloned().unwrap_or_default();
                self.candidates.push(Candidate {
                    package: package.clone(),
                    release: release.version.clone(),
                    order,
                    dependencies: Vec::new(),
                    provides: supplies.provides,
                    replaces: supplies.replaces,
                });
                order = order.saturating_add(1);
            }
            self.roots.push(RootRequirement {
                package,
                release: Some(release.version.clone()),
            });
        }
    }
}

/// Whether `error` is Modrinth reporting that what was asked for does not exist.
const fn is_not_found(error: &ModrinthError) -> bool {
    matches!(
        error,
        ModrinthError::Endpoint(EndpointError::Http(HttpError::Status { status: 404, .. }))
    )
}

/// A Modrinth API client over any [`HttpClient`].
pub struct Modrinth<'a> {
    endpoint: JsonEndpoint<'a>,
}

impl fmt::Debug for Modrinth<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Modrinth")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl<'a> Modrinth<'a> {
    /// A client for the production API.
    pub fn new(http: &'a dyn HttpClient) -> Self {
        Self::with_base(http, API_BASE)
    }

    /// A client for a manifest-validated deployment of the API, such as staging.
    pub(crate) fn with_base(http: &'a dyn HttpClient, base: impl Into<String>) -> Self {
        Self {
            endpoint: JsonEndpoint::new(http, base, METADATA_LIMIT),
        }
    }

    /// Searches for mods compatible with `target`.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for a request or decoding failure.
    pub fn search(
        &self,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchHit>, ModrinthError> {
        let mut facets = vec![
            vec!["project_type:mod".to_owned()],
            vec![format!("versions:{}", target.game_version)],
        ];
        let loader_ids: Vec<&str> = target.loader_ids().collect();
        if !loader_ids.is_empty() {
            facets.push(
                loader_ids
                    .iter()
                    .map(|loader| format!("categories:{loader}"))
                    .collect(),
            );
        }
        let facets = serde_json::to_string(&facets).unwrap_or_default();
        let limit = limit.min(MAX_SEARCH_LIMIT).to_string();
        let results: SearchResults = self.get_json(
            "/search",
            &[("query", query), ("facets", &facets), ("limit", &limit)],
        )?;
        Ok(results
            .hits
            .into_iter()
            .filter(|hit| target.supports_side(hit.client_side, hit.server_side))
            .collect())
    }

    /// Fetches a project by slug or id.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an invalid reference, a request failure or bad JSON.
    pub fn project(&self, reference: &str) -> Result<Project, ModrinthError> {
        self.get_json(&format!("/project/{}", segment(reference)?), &[])
    }

    /// Lists a project's versions compatible with `target`, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an invalid reference, a request failure or bad JSON.
    pub fn versions(&self, project: &str, target: &Target) -> Result<Vec<Version>, ModrinthError> {
        let loader_ids: Vec<&str> = target.loader_ids().collect();
        let loaders = serde_json::to_string(&loader_ids).unwrap_or_default();
        let game_versions =
            serde_json::to_string(std::slice::from_ref(&target.game_version)).unwrap_or_default();
        let mut versions: Vec<Version> = self.get_json(
            &format!("/project/{}/version", segment(project)?),
            &[
                ("loaders", &loaders),
                ("game_versions", &game_versions),
                ("include_changelog", "false"),
            ],
        )?;
        versions.retain(|version| supports(version, target));
        versions.sort_by(|a, b| b.date_published.cmp(&a.date_published));
        Ok(versions)
    }

    /// Fetches one version by id.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an invalid id, a request failure or bad JSON.
    pub fn version(&self, id: &str) -> Result<Version, ModrinthError> {
        self.get_json(&format!("/version/{}", segment(id)?), &[])
    }

    /// Chooses the version of `spec` to install: the pinned one if given, otherwise the newest
    /// release, otherwise the newest version of any type.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError::NoMatchingVersion`] if nothing compatible matches.
    pub fn select(&self, spec: &Spec, target: &Target) -> Result<Selection, ModrinthError> {
        let project = self.project(&spec.project)?;
        if !target.supports_side(project.client_side, project.server_side) {
            return Err(ModrinthError::UnsupportedSide {
                project: project.slug,
                side: target.side,
            });
        }
        let versions = self.versions(&project.id, target)?;
        let version = choose(&versions, spec.version.as_deref())
            .cloned()
            .ok_or_else(|| ModrinthError::NoMatchingVersion {
                project: project.slug.clone(),
                wanted: spec.version.clone(),
                loaders: target.loader_ids().map(str::to_owned).collect(),
                game_version: target.game_version.clone(),
            })?;
        let file = primary_file(&version)?.clone();
        Ok(Selection {
            project,
            version,
            file,
            required_by: None,
        })
    }

    /// Selects `specs` and, when `with_dependencies` is set, every required dependency,
    /// transitively.
    ///
    /// All reachable, target-compatible provider versions are collected before core PubGrub
    /// resolves exact release requirements. Provider dependency ranges from artifact metadata
    /// are added by the later Fabric/NeoForge metadata reader.
    ///
    /// `overlay` says which projects provide or replace others. A requirement on a project is met
    /// by a selected or installed project that provides or replaces it, so every such project is
    /// collected as a candidate too; one that no longer exists on Modrinth is skipped. `installed`
    /// releases stay as they are, so a selection that contradicts one, such as a second
    /// implementation of an API already installed, fails resolution with an explanation.
    ///
    /// # Errors
    ///
    /// Returns the first selection or request error.
    pub fn plan_install(
        &self,
        specs: &[Spec],
        target: &Target,
        with_dependencies: bool,
        overlay: &Overlay,
        installed: &[InstalledRelease],
    ) -> Result<InstallPlan, ModrinthError> {
        let mut graph = self.collect_candidates(specs, target, with_dependencies, overlay)?;
        graph.keep_installed(installed, overlay);
        let solved = solve(&graph.roots, &graph.candidates)?;
        self.install_plan(graph, &solved, with_dependencies)
    }

    fn collect_candidates(
        &self,
        specs: &[Spec],
        target: &Target,
        with_dependencies: bool,
        overlay: &Overlay,
    ) -> Result<CandidateGraph, ModrinthError> {
        let mut queue = VecDeque::new();
        let mut projects = BTreeMap::new();
        let mut graph = CandidateGraph::default();
        for spec in specs {
            let project = self.project(&spec.project)?;
            if !target.supports_side(project.client_side, project.server_side) {
                return Err(ModrinthError::UnsupportedSide {
                    project: project.slug,
                    side: target.side,
                });
            }
            graph.roots.push(RootRequirement {
                package: package(&project.id),
                release: spec.version.clone(),
            });
            graph.requested.push(project.id.clone());
            queue.push_back((project.id.clone(), Reach::Required));
            projects.insert(project.id.clone(), project);
        }

        let mut seen = BTreeSet::new();
        while let Some((project_id, reach)) = queue.pop_front() {
            if seen.contains(&project_id) {
                continue;
            }
            let Some(project) = self.queued_project(&project_id, reach, &mut projects)? else {
                continue;
            };
            seen.insert(project_id);
            let supplies = overlay
                .entry(&package(&project.id))
                .cloned()
                .unwrap_or_default();
            let versions = self.versions(&project.id, target)?;
            for (index, version) in versions.into_iter().enumerate() {
                let index = u64::try_from(index).map_err(|_| ModrinthError::TooManyCandidates)?;
                let dependencies = if with_dependencies {
                    self.solver_dependencies(&version, overlay, &mut queue)?
                } else {
                    Vec::new()
                };
                graph.candidates.push(Candidate {
                    package: package(&project.id),
                    release: version.id.clone(),
                    order: u64::MAX - index,
                    dependencies,
                    provides: supplies.provides.clone(),
                    replaces: supplies.replaces.clone(),
                });
                graph.records.insert(
                    (project.id.clone(), version.id.clone()),
                    (project.clone(), version),
                );
            }
        }
        graph.pin_requested_versions();
        Ok(graph)
    }

    /// The project `id` names, fetched at most once. A possible stand-in that Modrinth no longer
    /// has is `None`, so a stale overlay entry cannot fail resolution.
    fn queued_project(
        &self,
        id: &str,
        reach: Reach,
        projects: &mut BTreeMap<String, Project>,
    ) -> Result<Option<Project>, ModrinthError> {
        if let Some(project) = projects.get(id) {
            return Ok(Some(project.clone()));
        }
        match self.project(id) {
            Ok(project) => {
                projects.insert(id.to_owned(), project.clone());
                Ok(Some(project))
            }
            Err(error) if reach == Reach::StandIn && is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn install_plan(
        &self,
        graph: CandidateGraph,
        solved: &Solution,
        with_dependencies: bool,
    ) -> Result<InstallPlan, ModrinthError> {
        let CandidateGraph {
            requested,
            mut records,
            kept,
            ..
        } = graph;
        let mut selected: BTreeMap<String, (Project, Version)> = BTreeMap::new();
        for (package, candidate) in &solved.selected {
            let key = (package.project.clone(), candidate.release.clone());
            match records.remove(&key) {
                Some(record) => {
                    selected.insert(package.project.clone(), record);
                }
                None if kept.contains(&key) => {}
                None => return Err(ModrinthError::MissingCandidate(key.0, key.1)),
            }
        }
        let mut required_by = self.required_by(&selected, solved)?;
        let mut plan = InstallPlan::default();
        let mut ordered = requested;
        let transitive: Vec<String> = selected
            .keys()
            .filter(|project| !ordered.contains(project))
            .cloned()
            .collect();
        ordered.extend(transitive);
        for project_id in ordered {
            let Some((project, version)) = selected.remove(&project_id) else {
                continue;
            };
            self.record_relationships(&mut plan, &project, &version, solved, with_dependencies)?;
            let file = primary_file(&version)?.clone();
            plan.selections.push(Selection {
                project,
                version,
                file,
                required_by: required_by.remove(&project_id),
            });
        }
        Ok(plan)
    }

    /// For each selected project that meets another selected project's requirement, directly or
    /// by standing in for the required project, the slug of the first project requiring it.
    fn required_by(
        &self,
        selected: &BTreeMap<String, (Project, Version)>,
        solved: &Solution,
    ) -> Result<BTreeMap<String, String>, ModrinthError> {
        let mut required_by = BTreeMap::new();
        for (project, version) in selected.values() {
            for dependency in &version.dependencies {
                if dependency.dependency_type != DependencyType::Required {
                    continue;
                }
                let Some(required) = self.dependency_project(dependency)? else {
                    continue;
                };
                let required = package(&required);
                let meeting = solved.supplied_by.get(&required).unwrap_or(&required);
                if selected.contains_key(&meeting.project) {
                    required_by
                        .entry(meeting.project.clone())
                        .or_insert_with(|| project.slug.clone());
                }
            }
        }
        Ok(required_by)
    }

    /// Records the requirements of `version` that nothing selected or installed meets, when
    /// dependencies were not walked; those another project stands in for; and the selected or
    /// installed projects it declares incompatible.
    fn record_relationships(
        &self,
        plan: &mut InstallPlan,
        project: &Project,
        version: &Version,
        solved: &Solution,
        with_dependencies: bool,
    ) -> Result<(), ModrinthError> {
        for dependency in &version.dependencies {
            let Some(related) = self.dependency_project(dependency)? else {
                continue;
            };
            let present = solved.selected.contains_key(&package(&related));
            let stand_in = solved.supplied_by.get(&package(&related));
            let requirement = Requirement {
                project_id: related,
                declared_by: project.slug.clone(),
            };
            match (dependency.dependency_type, stand_in) {
                (DependencyType::Required, Some(stand_in)) => {
                    plan.substitutions.push(Substitution {
                        project_id: requirement.project_id,
                        declared_by: requirement.declared_by,
                        supplied_by: stand_in.project.clone(),
                    });
                }
                (DependencyType::Required, None) if !with_dependencies && !present => {
                    plan.unresolved.push(requirement);
                }
                (DependencyType::Incompatible, _) if present => {
                    plan.incompatible.push(requirement);
                }
                (
                    DependencyType::Required
                    | DependencyType::Incompatible
                    | DependencyType::Optional
                    | DependencyType::Embedded
                    | DependencyType::Unknown,
                    _,
                ) => {}
            }
        }
        Ok(())
    }

    /// The solver requirements `version` declares. Each required project is queued for
    /// collection, and so is every project the overlay says could stand in for it.
    fn solver_dependencies(
        &self,
        version: &Version,
        overlay: &Overlay,
        queue: &mut VecDeque<(String, Reach)>,
    ) -> Result<Vec<SolverRequirement>, ModrinthError> {
        let mut requirements = Vec::new();
        for dependency in &version.dependencies {
            if dependency.dependency_type != DependencyType::Required {
                continue;
            }
            let Some(project) = self.dependency_project(dependency)? else {
                continue;
            };
            let required = package(&project);
            queue.push_back((project, Reach::Required));
            if dependency.version_id.is_none() {
                queue.extend(
                    overlay
                        .suppliers(&required)
                        .filter(|supplier| supplier.provider == crate::MODRINTH)
                        .map(|supplier| (supplier.project.clone(), Reach::StandIn)),
                );
            }
            requirements.push(SolverRequirement {
                package: required,
                release: dependency.version_id.clone(),
            });
        }
        Ok(requirements)
    }

    /// Downloads `file` into `dir`, verifying its size and SHA-512 before returning its path.
    ///
    /// Only `https` URLs are accepted, and the file name must be a single safe component. A file
    /// that fails verification is left in `dir`, which the caller owns and discards.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for an unsafe URL or name, a transfer failure, or a mismatch.
    pub fn download(&self, file: &VersionFile, dir: &Path) -> Result<PathBuf, ModrinthError> {
        let descriptor = ArtifactDescriptor {
            url: file.url.clone(),
            file_name: file.filename.clone(),
            limit: file.size,
            size: Some(file.size),
            sha256: None,
            sha512: Some(file.hashes.sha512.clone()),
        };
        Ok(acquire(self.endpoint.http(), &descriptor, dir)?.path)
    }

    /// Checks installed files, identified by the SHA-512 Modrinth published for them, for
    /// versions to replace them with. The result is keyed by lowercase hash.
    ///
    /// A file stays on its release channel or moves to a more stable one: a release is only
    /// replaced by a release, and a beta by a beta or a release. A replacement is always newer
    /// than the installed version, unless the installed version does not support `target`; then
    /// the newest compatible version on its channel is offered, even if it is older.
    ///
    /// Costs one request for the installed versions and one per channel in use, however many
    /// files are checked.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] for a request or decoding failure, or a replacement without
    /// files.
    pub fn check_updates(
        &self,
        sha512s: &[String],
        target: &Target,
    ) -> Result<BTreeMap<String, UpdateCheck>, ModrinthError> {
        let hashes: BTreeSet<String> = sha512s
            .iter()
            .map(|hash| hash.to_ascii_lowercase())
            .collect();
        if hashes.is_empty() {
            return Ok(BTreeMap::new());
        }
        let all: Vec<&str> = hashes.iter().map(String::as_str).collect();
        let loader_ids: Vec<String> = target.loader_ids().map(str::to_owned).collect();
        let installed: BTreeMap<String, Version> = lowercase_keys(self.post_json(
            "/version_files",
            &HashQuery {
                hashes: &all,
                algorithm: "sha512",
                loaders: None,
                game_versions: None,
                version_types: None,
            },
        )?);

        let mut channels: BTreeMap<&'static [&'static str], Vec<&str>> = BTreeMap::new();
        for (hash, version) in &installed {
            channels
                .entry(channel(version.version_type))
                .or_default()
                .push(hash);
        }
        let mut latest = BTreeMap::new();
        for (version_types, group) in channels {
            let found: BTreeMap<String, Version> = self.post_json(
                "/version_files/update",
                &HashQuery {
                    hashes: &group,
                    algorithm: "sha512",
                    loaders: Some(&loader_ids),
                    game_versions: Some([target.game_version.as_str()]),
                    version_types: Some(version_types),
                },
            )?;
            latest.extend(lowercase_keys(found));
        }

        hashes
            .into_iter()
            .map(|hash| {
                let check = match installed.get(&hash) {
                    Some(version) => decide(version, latest.get(&hash), target)?,
                    None => UpdateCheck::Unlisted,
                };
                Ok((hash, check))
            })
            .collect()
    }

    /// The projects `version` requires and the ones it declares incompatible, attributed to
    /// `declared_by`. A dependency that names only a version is looked up to find its project.
    ///
    /// # Errors
    ///
    /// Returns [`ModrinthError`] if such a lookup fails.
    pub fn relationships(
        &self,
        version: &Version,
        declared_by: &str,
    ) -> Result<Relationships, ModrinthError> {
        let mut found = Relationships::default();
        for dependency in &version.dependencies {
            let list = match dependency.dependency_type {
                DependencyType::Required => &mut found.required,
                DependencyType::Incompatible => &mut found.incompatible,
                DependencyType::Optional | DependencyType::Embedded | DependencyType::Unknown => {
                    continue;
                }
            };
            if let Some(project_id) = self.dependency_project(dependency)? {
                list.push(Requirement {
                    project_id,
                    declared_by: declared_by.to_owned(),
                });
            }
        }
        Ok(found)
    }

    /// The project a dependency refers to, looking it up when only a version is named.
    fn dependency_project(&self, dependency: &Dependency) -> Result<Option<String>, ModrinthError> {
        Ok(match (&dependency.project_id, &dependency.version_id) {
            (Some(id), _) => Some(id.clone()),
            (None, Some(version)) => Some(self.version(version)?.project_id),
            (None, None) => None,
        })
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ModrinthError> {
        self.endpoint.get(path, query).map_err(Into::into)
    }

    fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
    ) -> Result<T, ModrinthError> {
        self.endpoint.post(path, request).map_err(Into::into)
    }
}

/// Why a Modrinth operation failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModrinthError {
    /// The target-filtered dependency graph could not be resolved.
    #[error(transparent)]
    Solver(#[from] msbe_core::solver::SolverError),
    /// A request through the configured metadata endpoint failed.
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    /// The request failed.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// A response did not have the expected shape.
    #[error("unexpected response from {url}: {reason}")]
    Decode {
        /// The request URL.
        url: String,
        /// The decoder's message.
        reason: String,
    },

    /// A project or version reference is malformed.
    #[error("{0:?} is not a valid Modrinth project or version reference")]
    InvalidSpec(String),

    /// A provider result set exceeded the supported address space.
    #[error("too many versions to rank for dependency resolution")]
    TooManyCandidates,

    /// PubGrub selected a candidate absent from the adapter's result cache.
    #[error("solver selected unavailable Modrinth version {1:?} for project {0:?}")]
    MissingCandidate(String, String),

    /// No version matched the request and the target.
    #[error(
        "no {} version of {project} supports loaders {loaders:?} on game version {game_version}",
        .wanted.as_deref().map_or("compatible", |_| "matching")
    )]
    NoMatchingVersion {
        /// The project slug.
        project: String,
        /// The pinned version, if any.
        wanted: Option<String>,
        /// The acceptable loaders.
        loaders: Vec<String>,
        /// The game version.
        game_version: String,
    },

    /// A project does not support the target's game side.
    #[error("project {project:?} does not support the {side:?} target")]
    UnsupportedSide {
        /// The project slug.
        project: String,
        /// The requested game side.
        side: Side,
    },

    /// A version has no files.
    #[error("version {version} has no files")]
    NoFiles {
        /// The version id.
        version: String,
    },

    /// A download's file name is not a single safe path component.
    #[error("refusing to write a download named {0:?}")]
    UnsafeFileName(String),

    /// A download URL is not `https`.
    #[error("refusing to download over an insecure URL: {0}")]
    InsecureUrl(String),

    /// A download is not the size Modrinth published.
    #[error("{file} is {actual} bytes, but Modrinth published {expected}")]
    SizeMismatch {
        /// The file name.
        file: String,
        /// The published size.
        expected: u64,
        /// The downloaded size.
        actual: u64,
    },

    /// A download does not match its published SHA-512.
    #[error("{file} failed SHA-512 verification: expected {expected}, got {actual}")]
    HashMismatch {
        /// The file name.
        file: String,
        /// The published hash.
        expected: String,
        /// The hash of what was downloaded.
        actual: String,
    },

    /// A download could not be written.
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        /// The destination.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl From<AcquisitionError> for ModrinthError {
    fn from(error: AcquisitionError) -> Self {
        match error {
            AcquisitionError::Transfer(error) => match error {
                crate::artifact::ArtifactError::Http(error) => Self::Http(error),
                crate::artifact::ArtifactError::Io { path, source } => Self::Io { path, source },
            },
            AcquisitionError::InsecureUrl(url) => Self::InsecureUrl(url),
            AcquisitionError::UnsafeFileName(file) => Self::UnsafeFileName(file),
            AcquisitionError::SizeMismatch {
                file,
                expected,
                actual,
            } => Self::SizeMismatch {
                file,
                expected,
                actual,
            },
            AcquisitionError::HashMismatch {
                file,
                algorithm: "SHA-512",
                expected,
                actual,
            } => Self::HashMismatch {
                file,
                expected,
                actual,
            },
            AcquisitionError::HashMismatch { .. } => unreachable!("Modrinth verifies SHA-512"),
        }
    }
}

/// Whether `raw` can be a slug, id or version number, and is safe as one URL path segment.
fn is_reference(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && raw.chars().any(|c| c.is_ascii_alphanumeric())
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

fn package(project: &str) -> PackageId {
    PackageId {
        provider: crate::MODRINTH.to_owned(),
        project: project.to_owned(),
    }
}

fn segment(raw: &str) -> Result<&str, ModrinthError> {
    if is_reference(raw) {
        Ok(raw)
    } else {
        Err(ModrinthError::InvalidSpec(raw.to_owned()))
    }
}

fn choose<'v>(versions: &'v [Version], wanted: Option<&str>) -> Option<&'v Version> {
    match wanted {
        Some(wanted) => versions
            .iter()
            .find(|version| version.id == wanted || version.version_number == wanted),
        None => versions
            .iter()
            .find(|version| version.version_type == VersionType::Release)
            .or_else(|| versions.first()),
    }
}

fn primary_file(version: &Version) -> Result<&VersionFile, ModrinthError> {
    version
        .files
        .iter()
        .find(|file| file.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| ModrinthError::NoFiles {
            version: version.id.clone(),
        })
}

/// The version types a file installed from `installed` may move to: its own, or more stable.
const fn channel(installed: VersionType) -> &'static [&'static str] {
    match installed {
        VersionType::Release => &["release"],
        VersionType::Beta => &["release", "beta"],
        VersionType::Alpha | VersionType::Unknown => &["release", "beta", "alpha"],
    }
}

fn supports(version: &Version, target: &Target) -> bool {
    let loader_ids: Vec<&str> = target.loader_ids().collect();
    let loader_matches = version
        .loaders
        .iter()
        .any(|loader| loader_ids.contains(&loader.as_str()));
    let version_matches = target.loader_version.as_ref().is_none_or(|wanted| {
        let declared: Vec<&String> = loader_ids
            .iter()
            .filter_map(|loader| version.loader_versions.get(*loader))
            .flatten()
            .collect();
        declared.is_empty() || declared.contains(&wanted)
    });
    version.game_versions.contains(&target.game_version) && loader_matches && version_matches
}

/// Whether `latest`, the newest version on the installed version's channel, should replace it.
fn decide(
    installed: &Version,
    latest: Option<&Version>,
    target: &Target,
) -> Result<UpdateCheck, ModrinthError> {
    let fits = supports(installed, target);
    let replacement = latest.filter(|candidate| {
        candidate.id != installed.id
            && supports(candidate, target)
            && (!fits || candidate.date_published > installed.date_published)
    });
    Ok(match replacement {
        Some(version) => UpdateCheck::Available(Box::new(Update {
            installed: installed.clone(),
            file: primary_file(version)?.clone(),
            version: version.clone(),
        })),
        None if fits => UpdateCheck::Current(Box::new(installed.clone())),
        None => UpdateCheck::Incompatible(Box::new(installed.clone())),
    })
}

fn lowercase_keys<V>(map: BTreeMap<String, V>) -> BTreeMap<String, V> {
    map.into_iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, io::Write};

    use msbe_plan_schema::Side;
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha512};

    use super::{
        InstalledRelease, Modrinth, ModrinthError, Requirement, Spec, Substitution, Target,
        UpdateCheck, Version, VersionType,
    };
    use crate::{
        Overlay,
        hashing::hex,
        http::{HttpClient, HttpError},
    };

    const BASE: &str = "https://api.modrinth.com/v2";

    #[derive(Default)]
    struct FakeHttp {
        json: BTreeMap<String, Value>,
        files: BTreeMap<String, Vec<u8>>,
        requests: RefCell<Vec<String>>,
    }

    impl FakeHttp {
        fn route(&mut self, path: &str, body: Value) {
            self.json.insert(format!("{BASE}{path}"), body);
        }

        /// Answers a POST to `path` that asks for `version_types` (comma-separated, or empty
        /// when the request names none).
        fn route_post(&mut self, path: &str, version_types: &str, body: Value) {
            self.json
                .insert(format!("{BASE}{path}#{version_types}"), body);
        }

        fn answer(&self, key: &str, url: &str) -> Result<Vec<u8>, HttpError> {
            self.json
                .get(key)
                .map(|body| serde_json::to_vec(body).unwrap())
                .ok_or_else(|| HttpError::Status {
                    url: url.to_owned(),
                    status: 404,
                })
        }
    }

    impl HttpClient for FakeHttp {
        fn get(
            &self,
            url: &str,
            query: &[(&str, &str)],
            _limit: u64,
        ) -> Result<Vec<u8>, HttpError> {
            let rendered: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            self.requests
                .borrow_mut()
                .push(format!("{url}?{}", rendered.join("&")));
            self.answer(url, url)
        }

        fn post_json(&self, url: &str, body: &[u8], _limit: u64) -> Result<Vec<u8>, HttpError> {
            let request: Value = serde_json::from_slice(body).unwrap();
            let version_types = request
                .get("version_types")
                .and_then(Value::as_array)
                .map(|types| {
                    types
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            self.requests
                .borrow_mut()
                .push(format!("POST {url} {request}"));
            self.answer(&format!("{url}#{version_types}"), url)
        }

        fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64, HttpError> {
            let bytes = self.files.get(url).ok_or_else(|| HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })?;
            let size = u64::try_from(bytes.len()).unwrap();
            if size > limit {
                return Err(HttpError::TooLarge {
                    url: url.to_owned(),
                    limit,
                });
            }
            sink.write_all(bytes).unwrap();
            Ok(size)
        }
    }

    fn contents(file: &str) -> Vec<u8> {
        format!("contents of {file}").into_bytes()
    }

    fn target() -> Target {
        Target {
            loader: "fabric".to_owned(),
            provides: Vec::new(),
            loader_version: None,
            game_version: "1.21.1".to_owned(),
            side: Side::Client,
        }
    }

    /// A version as Modrinth returns it, including fields this client ignores.
    fn version(id: &str, project: &str, number: &str, kind: &str, date: &str, file: &str) -> Value {
        let bytes = contents(file);
        json!({
            "id": id,
            "project_id": project,
            "version_number": number,
            "version_type": kind,
            "date_published": date,
            "loaders": ["fabric"],
            "game_versions": ["1.21.1"],
            "author_id": "ignored",
            "changelog": null,
            "downloads": 10,
            "files": [{
                "hashes": { "sha512": hex(&Sha512::digest(&bytes)), "sha1": "unused" },
                "url": format!("https://cdn.modrinth.test/{file}"),
                "filename": file,
                "primary": true,
                "size": bytes.len(),
                "file_type": null
            }],
            "dependencies": []
        })
    }

    /// Sodium (a newer beta and an older release) and Iris, which requires Sodium.
    fn catalogue() -> FakeHttp {
        let mut http = FakeHttp::default();
        let sodium = json!({ "id": "AANobbMI", "slug": "sodium", "title": "Sodium", "project_type": "mod",
                    "client_side": "required", "server_side": "required" });
        http.route("/project/sodium", sodium.clone());
        http.route("/project/AANobbMI", sodium);
        http.route(
            "/project/AANobbMI/version",
            json!([
                version(
                    "S1",
                    "AANobbMI",
                    "0.8.12",
                    "release",
                    "2026-07-06T00:00:00Z",
                    "sodium-0.8.12.jar"
                ),
                version(
                    "S2",
                    "AANobbMI",
                    "0.8.13-beta.1",
                    "beta",
                    "2026-08-07T00:00:00Z",
                    "sodium-beta.jar"
                ),
            ]),
        );
        let mut iris = version(
            "I1",
            "YL57xq9U",
            "1.8.0",
            "release",
            "2026-08-01T00:00:00Z",
            "iris-1.8.0.jar",
        );
        iris.as_object_mut().unwrap().insert(
            "dependencies".to_owned(),
            json!([{ "project_id": "AANobbMI", "version_id": null, "dependency_type": "required" }]),
        );
        http.route(
            "/project/iris",
            json!({ "id": "YL57xq9U", "slug": "iris", "title": "Iris", "project_type": "mod",
                    "client_side": "required", "server_side": "required" }),
        );
        http.route("/project/YL57xq9U/version", json!([iris]));
        for file in ["sodium-0.8.12.jar", "sodium-beta.jar", "iris-1.8.0.jar"] {
            http.files
                .insert(format!("https://cdn.modrinth.test/{file}"), contents(file));
        }
        http
    }

    #[test]
    fn specs_accept_slugs_ids_and_pins_and_reject_path_characters() {
        assert_eq!(
            Spec::parse("sodium").unwrap(),
            Spec {
                project: "sodium".to_owned(),
                version: None
            }
        );
        assert_eq!(
            Spec::parse("sodium@mc1.21.1-0.8.13+fabric")
                .unwrap()
                .version
                .as_deref(),
            Some("mc1.21.1-0.8.13+fabric")
        );
        for bad in ["", "@1.0", "sodium@", "../etc", "a/b", "..", "sodium@1 0"] {
            assert!(Spec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn select_prefers_the_newest_release_and_honours_pins() {
        let http = catalogue();
        let modrinth = Modrinth::new(&http);
        let chosen = modrinth
            .select(&Spec::parse("sodium").unwrap(), &target())
            .unwrap();
        assert_eq!(chosen.version.version_number, "0.8.12");
        assert_eq!(chosen.version.version_type, VersionType::Release);

        let pinned = modrinth
            .select(&Spec::parse("sodium@0.8.13-beta.1").unwrap(), &target())
            .unwrap();
        assert_eq!(pinned.version.id, "S2");

        assert!(matches!(
            modrinth.select(&Spec::parse("sodium@9.9.9").unwrap(), &target()),
            Err(ModrinthError::NoMatchingVersion { .. })
        ));
        assert!(http.requests.borrow().iter().any(|request| {
            request.contains("/project/AANobbMI/version?")
                && request.contains(r#"loaders=["fabric"]"#)
                && request.contains(r#"game_versions=["1.21.1"]"#)
                && request.contains("include_changelog=false")
        }));
    }

    #[test]
    fn target_prefilter_honours_loader_capabilities_side_and_loader_version() {
        let mut http = catalogue();
        let mut quilt = target();
        quilt.loader = "quilt".to_owned();
        quilt.provides = vec!["fabric".to_owned()];
        assert!(
            Modrinth::new(&http)
                .select(&Spec::parse("sodium").unwrap(), &quilt)
                .is_ok()
        );

        let server_only = http
            .json
            .get_mut(&format!("{BASE}/project/sodium"))
            .and_then(Value::as_object_mut);
        if let Some(project) = server_only {
            project.insert("server_side".to_owned(), json!("unsupported"));
        }
        let mut server = target();
        server.side = Side::Server;
        assert!(matches!(
            Modrinth::new(&http).select(&Spec::parse("sodium").unwrap(), &server),
            Err(ModrinthError::UnsupportedSide { .. })
        ));

        let versions = http
            .json
            .get_mut(&format!("{BASE}/project/AANobbMI/version"))
            .and_then(Value::as_array_mut);
        if let Some(versions) = versions {
            for version in versions {
                if let Some(version) = version.as_object_mut() {
                    version.insert("loader_versions".to_owned(), json!({ "fabric": ["0.16"] }));
                }
            }
        }
        let mut mismatched = target();
        mismatched.loader_version = Some("0.17".to_owned());
        assert!(matches!(
            Modrinth::new(&http).select(&Spec::parse("sodium").unwrap(), &mismatched),
            Err(ModrinthError::NoMatchingVersion { .. })
        ));
        mismatched.loader_version = Some("0.16".to_owned());
        assert!(
            Modrinth::new(&http)
                .select(&Spec::parse("sodium").unwrap(), &mismatched)
                .is_ok()
        );
    }

    #[test]
    fn required_dependencies_are_walked_only_when_asked() {
        let http = catalogue();
        let modrinth = Modrinth::new(&http);
        let iris = [Spec::parse("iris").unwrap()];

        let with = modrinth
            .plan_install(&iris, &target(), true, &Overlay::default(), &[])
            .unwrap();
        let chosen: Vec<(&str, Option<&str>)> = with
            .selections
            .iter()
            .map(|selection| {
                (
                    selection.project.slug.as_str(),
                    selection.required_by.as_deref(),
                )
            })
            .collect();
        assert_eq!(chosen, [("iris", None), ("sodium", Some("iris"))]);
        assert!(with.unresolved.is_empty());

        let without = modrinth
            .plan_install(&iris, &target(), false, &Overlay::default(), &[])
            .unwrap();
        assert_eq!(without.selections.len(), 1);
        let [missing] = without.unresolved.as_slice() else {
            panic!(
                "expected one unresolved requirement, got {:?}",
                without.unresolved
            );
        };
        assert_eq!(
            (missing.project_id.as_str(), missing.declared_by.as_str()),
            ("AANobbMI", "iris")
        );
    }

    #[test]
    fn dependency_solver_backtracks_to_satisfy_an_exact_transitive_release() {
        let mut http = catalogue();
        let sodium_versions = http
            .json
            .get_mut(&format!("{BASE}/project/AANobbMI/version"))
            .and_then(Value::as_array_mut);
        let Some(sodium_versions) = sodium_versions else {
            panic!("sodium version fixture is missing");
        };
        sodium_versions.push(version(
            "S3",
            "AANobbMI",
            "0.9.0",
            "release",
            "2026-09-01T00:00:00Z",
            "sodium-0.9.0.jar",
        ));
        let iris_versions = http
            .json
            .get_mut(&format!("{BASE}/project/YL57xq9U/version"))
            .and_then(Value::as_array_mut);
        let Some(iris_versions) = iris_versions else {
            panic!("iris version fixture is missing");
        };
        let Some(iris) = iris_versions.first_mut() else {
            panic!("iris version fixture is empty");
        };
        iris["dependencies"] = json!([{
            "project_id": "AANobbMI", "version_id": "S1", "dependency_type": "required"
        }]);

        let plan = Modrinth::new(&http)
            .plan_install(
                &[Spec::parse("iris").unwrap(), Spec::parse("sodium").unwrap()],
                &target(),
                true,
                &Overlay::default(),
                &[],
            )
            .unwrap();
        let sodium = plan
            .selections
            .iter()
            .find(|selection| selection.project.id == "AANobbMI");
        let Some(sodium) = sodium else {
            panic!("sodium was not selected");
        };
        assert_eq!(sodium.version.id, "S1");
    }

    #[test]
    fn a_project_reached_twice_is_selected_once() {
        let http = catalogue();
        let plan = Modrinth::new(&http)
            .plan_install(
                &[Spec::parse("sodium").unwrap(), Spec::parse("iris").unwrap()],
                &target(),
                true,
                &Overlay::default(),
                &[],
            )
            .unwrap();
        let slugs: Vec<&str> = plan
            .selections
            .iter()
            .map(|selection| selection.project.slug.as_str())
            .collect();
        assert_eq!(slugs, ["sodium", "iris"]);
    }

    /// The catalogue, plus Fabric API, Quilted Fabric API, which provides it, and a newer Sodium
    /// that requires it. Forgified Fabric API, the other built-in stand-in for Fabric API, is
    /// absent, as if Modrinth had removed it.
    fn fabric_api_catalogue() -> FakeHttp {
        let mut http = catalogue();
        for (id, slug) in [("P7dR8mSH", "fabric-api"), ("qvIfYCYJ", "qsl")] {
            http.route(
                &format!("/project/{id}"),
                json!({ "id": id, "slug": slug, "title": slug, "project_type": "mod",
                        "client_side": "required", "server_side": "required" }),
            );
            http.route(
                &format!("/project/{id}/version"),
                json!([version(
                    &format!("{slug}-1"),
                    id,
                    "1.0.0",
                    "release",
                    "2026-08-01T00:00:00Z",
                    &format!("{slug}.jar"),
                )]),
            );
        }
        let mut sodium = version(
            "S3",
            "AANobbMI",
            "0.9.0",
            "release",
            "2026-09-01T00:00:00Z",
            "sodium-0.9.0.jar",
        );
        sodium.as_object_mut().unwrap().insert(
            "dependencies".to_owned(),
            json!([{ "project_id": "P7dR8mSH", "version_id": null, "dependency_type": "required" }]),
        );
        let versions = http
            .json
            .get_mut(&format!("{BASE}/project/AANobbMI/version"))
            .and_then(Value::as_array_mut);
        let Some(versions) = versions else {
            panic!("sodium version fixture is missing");
        };
        versions.push(sodium);
        http
    }

    #[test]
    fn an_installed_stand_in_meets_requirements_and_excludes_the_original() {
        fn slugs(plan: &super::InstallPlan) -> Vec<&str> {
            plan.selections
                .iter()
                .map(|selection| selection.project.slug.as_str())
                .collect()
        }

        let http = fabric_api_catalogue();
        let modrinth = Modrinth::new(&http);
        let overlay = Overlay::builtins().unwrap();
        let sodium = [Spec::parse("sodium").unwrap()];

        // Nothing stands in yet, so Fabric API is preferred over the fork that provides it, and
        // the stand-in Modrinth no longer has does not fail resolution.
        let fresh = modrinth
            .plan_install(&sodium, &target(), true, &overlay, &[])
            .unwrap();
        assert_eq!(slugs(&fresh), ["sodium", "fabric-api"]);
        assert!(fresh.substitutions.is_empty());

        let quilted = [InstalledRelease {
            project: "qvIfYCYJ".to_owned(),
            version: "qsl-1".to_owned(),
        }];
        let substitution = Substitution {
            project_id: "P7dR8mSH".to_owned(),
            declared_by: "sodium".to_owned(),
            supplied_by: "qvIfYCYJ".to_owned(),
        };
        let kept = modrinth
            .plan_install(&sodium, &target(), true, &overlay, &quilted)
            .unwrap();
        assert_eq!(slugs(&kept), ["sodium", "qsl"]);
        assert_eq!(
            kept.selections
                .iter()
                .find(|selection| selection.project.slug == "qsl")
                .and_then(|selection| selection.required_by.as_deref()),
            Some("sodium")
        );
        assert_eq!(kept.substitutions, std::slice::from_ref(&substitution));

        // Without walking dependencies, the installed fork is never fetched, yet it still meets
        // the requirement instead of leaving it unresolved.
        let bare = modrinth
            .plan_install(&sodium, &target(), false, &overlay, &quilted)
            .unwrap();
        assert_eq!(slugs(&bare), ["sodium"]);
        assert!(bare.unresolved.is_empty(), "{:?}", bare.unresolved);
        assert_eq!(bare.substitutions, [substitution]);

        let fabric_api = [Spec::parse("P7dR8mSH").unwrap()];
        assert!(matches!(
            modrinth.plan_install(&fabric_api, &target(), false, &overlay, &quilted),
            Err(ModrinthError::Solver(_))
        ));
    }

    #[test]
    fn downloads_are_verified_before_they_are_trusted() {
        let mut http = catalogue();
        let sodium = Modrinth::new(&http)
            .select(&Spec::parse("sodium").unwrap(), &target())
            .unwrap();
        let good = tempfile::tempdir().unwrap();
        let path = Modrinth::new(&http)
            .download(&sodium.file, good.path())
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), contents("sodium-0.8.12.jar"));

        // Same size, one bit different: only the hash can catch it.
        let mut tampered = contents("sodium-0.8.12.jar");
        if let Some(last) = tampered.last_mut() {
            *last ^= 1;
        }
        http.files.insert(sodium.file.url.clone(), tampered);
        let bad = tempfile::tempdir().unwrap();
        assert!(matches!(
            Modrinth::new(&http).download(&sodium.file, bad.path()),
            Err(ModrinthError::HashMismatch { .. })
        ));

        let mut insecure = sodium.file.clone();
        insecure.url = insecure.url.replacen("https://", "http://", 1);
        assert!(matches!(
            Modrinth::new(&http).download(&insecure, bad.path()),
            Err(ModrinthError::InsecureUrl(_))
        ));

        let mut escaping = sodium.file;
        escaping.filename = "../escape.jar".to_owned();
        assert!(matches!(
            Modrinth::new(&http).download(&escaping, bad.path()),
            Err(ModrinthError::UnsafeFileName(_))
        ));
    }

    #[test]
    fn updates_stay_on_their_channel_and_only_go_back_to_regain_compatibility() {
        let hash = |c: char| c.to_string().repeat(128);
        let dated = |id: &str, kind: &str, date: &str, game_version: &str| {
            let mut entry = version(id, "P", id, kind, date, &format!("{id}.jar"));
            entry
                .as_object_mut()
                .unwrap()
                .insert("game_versions".to_owned(), json!([game_version]));
            entry
        };
        let mut http = FakeHttp::default();
        http.route_post(
            "/version_files",
            "",
            json!({
                hash('a'): dated("R1", "release", "2026-07-01T00:00:00Z", "1.21.1"),
                hash('b'): dated("B1", "beta", "2026-08-10T00:00:00Z", "1.21.1"),
                hash('c'): dated("O1", "release", "2026-09-01T00:00:00Z", "1.20.1"),
                hash('d'): dated("G1", "release", "2026-09-01T00:00:00Z", "1.20.1"),
            }),
        );
        http.route_post(
            "/version_files/update",
            "release",
            json!({
                // Newer on the release channel: an update.
                hash('a'): dated("R2", "release", "2026-08-01T00:00:00Z", "1.21.1"),
                // Older, but the installed version does not support 1.21.1: still offered.
                hash('c'): dated("O2", "release", "2026-01-01T00:00:00Z", "1.21.1"),
            }),
        );
        http.route_post(
            "/version_files/update",
            "release,beta",
            // Older than the installed beta, so not a downgrade target.
            json!({ hash('b'): dated("R3", "release", "2026-08-01T00:00:00Z", "1.21.1") }),
        );

        let checks = Modrinth::new(&http)
            .check_updates(
                &[hash('A'), hash('b'), hash('c'), hash('d'), hash('e')],
                &target(),
            )
            .unwrap();
        let available = |key: char| match checks.get(&hash(key)) {
            Some(UpdateCheck::Available(update)) => Some(update.version.id.as_str()),
            _ => None,
        };
        assert_eq!(available('a'), Some("R2"));
        assert!(
            matches!(checks.get(&hash('b')), Some(UpdateCheck::Current(installed)) if installed.id == "B1")
        );
        assert_eq!(available('c'), Some("O2"));
        assert!(matches!(
            checks.get(&hash('d')),
            Some(UpdateCheck::Incompatible(_))
        ));
        assert_eq!(checks.get(&hash('e')), Some(&UpdateCheck::Unlisted));

        let requests = http.requests.borrow();
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert!(
            requests.iter().any(|request| {
                request.contains(r#""version_types":["release","beta"]"#)
                    && request.contains(r#""loaders":["fabric"]"#)
                    && request.contains(r#""game_versions":["1.21.1"]"#)
                    && request.contains(r#""algorithm":"sha512""#)
            }),
            "{requests:?}"
        );
    }

    #[test]
    fn relationships_resolve_dependencies_that_name_only_a_version() {
        fn ids(list: &[Requirement]) -> Vec<&str> {
            list.iter()
                .map(|requirement| requirement.project_id.as_str())
                .collect()
        }

        let mut http = FakeHttp::default();
        http.route(
            "/version/V9",
            version(
                "V9",
                "QQQ",
                "2.0",
                "release",
                "2026-01-01T00:00:00Z",
                "q.jar",
            ),
        );
        let mut iris: Version = serde_json::from_value(version(
            "I1",
            "YL57xq9U",
            "1.8.0",
            "release",
            "2026-08-01T00:00:00Z",
            "iris.jar",
        ))
        .unwrap();
        iris.dependencies = serde_json::from_value(json!([
            { "project_id": "AANobbMI", "dependency_type": "required" },
            { "version_id": "V9", "dependency_type": "required" },
            { "project_id": "XXX", "dependency_type": "incompatible" },
            { "project_id": "YYY", "dependency_type": "optional" }
        ]))
        .unwrap();

        let found = Modrinth::new(&http).relationships(&iris, "iris").unwrap();
        assert_eq!(ids(&found.required), ["AANobbMI", "QQQ"]);
        assert_eq!(ids(&found.incompatible), ["XXX"]);
        assert!(found.required.iter().all(|r| r.declared_by == "iris"));
    }

    #[test]
    fn search_asks_for_mods_compatible_with_the_target() {
        let mut http = FakeHttp::default();
        http.route(
            "/search",
            json!({
                "hits": [{ "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
                           "description": "A rendering engine", "downloads": 42,
                           "client_side": "required", "server_side": "required", "author": "ignored" }],
                "offset": 0, "limit": 5, "total_hits": 1
            }),
        );
        let hits = Modrinth::new(&http).search("render", &target(), 5).unwrap();
        assert_eq!(hits.first().map(|hit| hit.slug.as_str()), Some("sodium"));

        let requests = http.requests.borrow();
        let request = requests.first().unwrap();
        assert!(
            request.contains(
                r#"facets=[["project_type:mod"],["versions:1.21.1"],["categories:fabric"]]"#
            ),
            "{request}"
        );
        assert!(request.contains("limit=5"), "{request}");
    }
}
