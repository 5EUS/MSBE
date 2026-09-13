//! Dependency resolution across any set of adapters.
//!
//! Adapters describe projects and releases; this module collects every target-compatible release
//! a request can reach, applies the overlay and the releases already installed, and hands the
//! result to the PubGrub solver in `msbe_core::solver`. Nothing here knows a provider, so a
//! requirement on one provider's project can be met by another provider's project when the
//! overlay says it stands in.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
};

use msbe_core::solver::{
    Candidate, PackageId, Requirement as SolverRequirement, RootRequirement, Solution, SolverError,
    solve,
};
use msbe_plan_schema::Side;
use serde::Serialize;
use thiserror::Error;

use crate::{
    Adapter, AdapterError, HttpClient, Overlay, Releases, Target,
    model::{Dependency, DependencyKind, Project, Release, Selection},
};

/// Finds the adapter serving a provider.
pub trait Adapters {
    /// The adapter for `provider`, when one is registered and permitted.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError::Unavailable`] when no permitted adapter serves `provider`.
    fn lookup(&self, provider: &str) -> Result<&dyn Adapter, ResolveError>;
}

/// A single adapter, for resolving with one provider at hand.
#[derive(Debug, Clone, Copy)]
pub struct Only<'a>(pub &'a dyn Adapter);

impl Adapters for Only<'_> {
    fn lookup(&self, provider: &str) -> Result<&dyn Adapter, ResolveError> {
        if provider == self.0.id() {
            Ok(self.0)
        } else {
            Err(ResolveError::Unavailable {
                provider: provider.to_owned(),
                reason: format!("only {} is available", self.0.id()),
            })
        }
    }
}

/// A project request routed to its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRequest {
    /// The provider id.
    pub provider: String,
    /// The provider's reference for the project, such as a slug or an id.
    pub reference: String,
    /// A release id or version number to pin, if any.
    pub version: Option<String>,
}

/// A release already in the profile, which resolution keeps as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRelease {
    /// The project.
    pub package: PackageId,
    /// The installed release id.
    pub release: String,
}

/// A declared relationship to a project, and the project that declared it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    /// The provider of the project referred to.
    pub provider: String,
    /// The project referred to.
    pub project_id: String,
    /// The slug of the project that declared it.
    pub declared_by: String,
}

impl Requirement {
    /// The identity of the project referred to.
    pub fn package(&self) -> PackageId {
        PackageId {
            provider: self.provider.clone(),
            project: self.project_id.clone(),
        }
    }
}

/// A required project that another selected or installed project provides or replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Substitution {
    /// The requirement that was met.
    #[serde(flatten)]
    pub requirement: Requirement,
    /// The project that stands in for the required one.
    pub supplied_by: PackageId,
}

/// What installing a set of projects involves.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct InstallPlan {
    /// Every project to install, requested ones first.
    pub selections: Vec<Selection>,
    /// Required dependencies that nothing selected or installed meets.
    pub unresolved: Vec<Requirement>,
    /// Declared incompatibilities with selected or installed projects.
    pub incompatible: Vec<Requirement>,
    /// Required projects that another selected or installed project stands in for.
    pub substitutions: Vec<Substitution>,
}

/// The projects a release requires, and the ones it declares incompatible.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Relationships {
    /// Projects that must be installed too.
    pub required: Vec<Requirement>,
    /// Projects that must not be installed alongside.
    pub incompatible: Vec<Requirement>,
}

/// Resolution against one target, over a set of adapters.
pub struct Resolver<'a> {
    /// The adapters requests and dependencies are routed to.
    pub adapters: &'a dyn Adapters,
    /// The client every adapter request goes through.
    pub http: &'a dyn HttpClient,
    /// What every selected release must be compatible with.
    pub target: &'a Target,
    /// Which projects provide or replace others.
    pub overlay: &'a Overlay,
}

impl fmt::Debug for Resolver<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Resolver")
            .field("target", self.target)
            .field("overlay", self.overlay)
            .finish_non_exhaustive()
    }
}

impl Resolver<'_> {
    /// Selects `requests` and, when `with_dependencies` is set, every required dependency,
    /// transitively.
    ///
    /// All reachable, target-compatible releases are collected before PubGrub resolves exact
    /// release requirements. A requirement on a project is also met by a selected or installed
    /// project the overlay says provides or replaces it, so every such project is collected as a
    /// candidate too; one whose provider is unavailable, or that its provider no longer has, is
    /// skipped. `installed` releases stay as they are, so a selection that contradicts one, such
    /// as a second implementation of an API already installed, fails with an explanation.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError`] for an unavailable provider, a request failure, a requested
    /// project that does not support the target's side, or requirements that cannot all be met.
    pub fn plan_install(
        &self,
        requests: &[ProjectRequest],
        with_dependencies: bool,
        installed: &[InstalledRelease],
    ) -> Result<InstallPlan, ResolveError> {
        let mut graph = self.collect_candidates(requests, with_dependencies)?;
        graph.keep_installed(installed, self.overlay);
        let solved = solve(&graph.roots, &graph.candidates)?;
        self.install_plan(graph, &solved, with_dependencies)
    }

    /// The projects `release` requires and the ones it declares incompatible, attributed to
    /// `declared_by`. A dependency that names only a release is looked up to find its project.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError`] if such a lookup fails.
    pub fn relationships(
        &self,
        release: &Release,
        declared_by: &str,
    ) -> Result<Relationships, ResolveError> {
        let mut found = Relationships::default();
        for dependency in &release.dependencies {
            let list = match dependency.kind {
                DependencyKind::Required => &mut found.required,
                DependencyKind::Incompatible => &mut found.incompatible,
                DependencyKind::Optional | DependencyKind::Embedded | DependencyKind::Unknown => {
                    continue;
                }
            };
            if let Some(project) = self.dependency_project(&release.project, dependency)? {
                list.push(requirement(&project, declared_by));
            }
        }
        Ok(found)
    }

    fn releases_of(&self, provider: &str) -> Result<&dyn Releases, ResolveError> {
        self.adapters
            .lookup(provider)?
            .as_releases()
            .ok_or_else(|| ResolveError::Unsupported {
                provider: provider.to_owned(),
                capability: "project releases",
            })
    }

    fn collect_candidates(
        &self,
        requests: &[ProjectRequest],
        with_dependencies: bool,
    ) -> Result<CandidateGraph, ResolveError> {
        let mut queue = VecDeque::new();
        let mut projects = BTreeMap::new();
        let mut graph = CandidateGraph::default();
        for request in requests {
            let project = self.releases_of(&request.provider)?.project(
                self.http,
                &request.reference,
                self.target,
            )?;
            if !self.target.supports_side(project.client, project.server) {
                return Err(ResolveError::UnsupportedSide {
                    project: project.label().to_owned(),
                    side: self.target.side,
                });
            }
            graph.roots.push(RootRequirement {
                package: project.id.clone(),
                release: request.version.clone(),
            });
            graph.requested.push(project.id.clone());
            queue.push_back((project.id.clone(), Reach::Required));
            projects.insert(project.id.clone(), project);
        }

        let mut seen = BTreeSet::new();
        while let Some((package, reach)) = queue.pop_front() {
            if seen.contains(&package) {
                continue;
            }
            let Some((project, releases)) = self.fetch(&package, reach, &mut projects)? else {
                continue;
            };
            seen.insert(package);
            let supplies = self.overlay.entry(&project.id).cloned().unwrap_or_default();
            for (index, release) in releases.into_iter().enumerate() {
                let index = u64::try_from(index).map_err(|_| ResolveError::TooManyCandidates)?;
                let dependencies = if with_dependencies {
                    self.solver_dependencies(&release, &mut queue)?
                } else {
                    Vec::new()
                };
                graph.candidates.push(Candidate {
                    package: project.id.clone(),
                    release: release.id.clone(),
                    order: u64::MAX - index,
                    dependencies,
                    provides: supplies.provides.clone(),
                    replaces: supplies.replaces.clone(),
                });
                graph.records.insert(
                    (project.id.clone(), release.id.clone()),
                    (project.clone(), release),
                );
            }
        }
        graph.pin_requested_versions();
        Ok(graph)
    }

    /// The project `package` names and its target-compatible releases, fetching the project at
    /// most once. A possible stand-in whose provider is unavailable, or that its provider no
    /// longer has, is `None`, so a stale overlay entry cannot fail resolution.
    fn fetch(
        &self,
        package: &PackageId,
        reach: Reach,
        projects: &mut BTreeMap<PackageId, Project>,
    ) -> Result<Option<Fetched>, ResolveError> {
        let releases = match self.releases_of(&package.provider) {
            Ok(releases) => releases,
            Err(_) if reach == Reach::StandIn => return Ok(None),
            Err(error) => return Err(error),
        };
        let project = if let Some(project) = projects.get(package) {
            project.clone()
        } else {
            match releases.project(self.http, &package.project, self.target) {
                Ok(project) => {
                    projects.insert(package.clone(), project.clone());
                    project
                }
                Err(error) if reach == Reach::StandIn && error.is_not_found() => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        };
        let found = releases.releases(self.http, &project.id.project, self.target)?;
        Ok(Some((project, found)))
    }

    fn install_plan(
        &self,
        graph: CandidateGraph,
        solved: &Solution,
        with_dependencies: bool,
    ) -> Result<InstallPlan, ResolveError> {
        let CandidateGraph {
            requested,
            mut records,
            kept,
            ..
        } = graph;
        let mut selected: BTreeMap<PackageId, ReleaseRecord> = BTreeMap::new();
        for (package, candidate) in &solved.selected {
            let key = (package.clone(), candidate.release.clone());
            match records.remove(&key) {
                Some(record) => {
                    selected.insert(package.clone(), record);
                }
                None if kept.contains(&key) => {}
                None => {
                    return Err(ResolveError::MissingCandidate {
                        package: key.0,
                        release: key.1,
                    });
                }
            }
        }
        let mut required_by = self.required_by(&selected, solved)?;
        let mut plan = InstallPlan::default();
        let mut ordered = requested;
        let transitive: Vec<PackageId> = selected
            .keys()
            .filter(|package| !ordered.contains(package))
            .cloned()
            .collect();
        ordered.extend(transitive);
        for package in ordered {
            let Some((project, release)) = selected.remove(&package) else {
                continue;
            };
            self.record_relationships(&mut plan, &project, &release, solved, with_dependencies)?;
            let file = release
                .primary_file()
                .cloned()
                .ok_or_else(|| AdapterError::NoFiles {
                    project: project.id.clone(),
                    release: release.id.clone(),
                })?;
            plan.selections.push(Selection {
                project,
                release,
                file,
                required_by: required_by.remove(&package),
            });
        }
        Ok(plan)
    }

    /// For each selected project that meets another selected project's requirement, directly or
    /// by standing in for the required project, the label of the first project requiring it.
    fn required_by(
        &self,
        selected: &BTreeMap<PackageId, ReleaseRecord>,
        solved: &Solution,
    ) -> Result<BTreeMap<PackageId, String>, ResolveError> {
        let mut required_by = BTreeMap::new();
        for (project, release) in selected.values() {
            for dependency in &release.dependencies {
                if dependency.kind != DependencyKind::Required {
                    continue;
                }
                let Some(required) = self.dependency_project(&release.project, dependency)? else {
                    continue;
                };
                let meeting = solved.supplied_by.get(&required).unwrap_or(&required);
                if selected.contains_key(meeting) {
                    required_by
                        .entry(meeting.clone())
                        .or_insert_with(|| project.label().to_owned());
                }
            }
        }
        Ok(required_by)
    }

    /// Records the requirements of `release` that nothing selected or installed meets, when
    /// dependencies were not walked; those another project stands in for; and the selected or
    /// installed projects it declares incompatible.
    fn record_relationships(
        &self,
        plan: &mut InstallPlan,
        project: &Project,
        release: &Release,
        solved: &Solution,
        with_dependencies: bool,
    ) -> Result<(), ResolveError> {
        for dependency in &release.dependencies {
            let Some(related) = self.dependency_project(&release.project, dependency)? else {
                continue;
            };
            let present = solved.selected.contains_key(&related);
            let stand_in = solved.supplied_by.get(&related).cloned();
            let requirement = requirement(&related, project.label());
            match (dependency.kind, stand_in) {
                (DependencyKind::Required, Some(supplied_by)) => {
                    plan.substitutions.push(Substitution {
                        requirement,
                        supplied_by,
                    });
                }
                (DependencyKind::Required, None) if !with_dependencies && !present => {
                    plan.unresolved.push(requirement);
                }
                (DependencyKind::Incompatible, _) if present => {
                    plan.incompatible.push(requirement);
                }
                (
                    DependencyKind::Required
                    | DependencyKind::Incompatible
                    | DependencyKind::Optional
                    | DependencyKind::Embedded
                    | DependencyKind::Unknown,
                    _,
                ) => {}
            }
        }
        Ok(())
    }

    /// The solver requirements `release` declares. Each required project is queued for
    /// collection, and so is every project the overlay says could stand in for it.
    fn solver_dependencies(
        &self,
        release: &Release,
        queue: &mut VecDeque<(PackageId, Reach)>,
    ) -> Result<Vec<SolverRequirement>, ResolveError> {
        let mut requirements = Vec::new();
        for dependency in &release.dependencies {
            if dependency.kind != DependencyKind::Required {
                continue;
            }
            let Some(required) = self.dependency_project(&release.project, dependency)? else {
                continue;
            };
            queue.push_back((required.clone(), Reach::Required));
            if dependency.release.is_none() {
                queue.extend(
                    self.overlay
                        .suppliers(&required)
                        .map(|supplier| (supplier.clone(), Reach::StandIn)),
                );
            }
            requirements.push(SolverRequirement {
                package: required,
                release: dependency.release.clone(),
            });
        }
        Ok(requirements)
    }

    /// The project a dependency refers to, asking the provider of the declaring release when
    /// only a release is named.
    fn dependency_project(
        &self,
        declaring: &PackageId,
        dependency: &Dependency,
    ) -> Result<Option<PackageId>, ResolveError> {
        Ok(match (&dependency.project, &dependency.release) {
            (Some(project), _) => Some(project.clone()),
            (None, Some(release)) => Some(self.releases_of(&declaring.provider)?.release_project(
                self.http,
                release,
                self.target,
            )?),
            (None, None) => None,
        })
    }
}

fn requirement(project: &PackageId, declared_by: &str) -> Requirement {
    Requirement {
        provider: project.provider.clone(),
        project_id: project.project.clone(),
        declared_by: declared_by.to_owned(),
    }
}

type ReleaseKey = (PackageId, String);
type ReleaseRecord = (Project, Release);
/// A project and its target-compatible releases.
type Fetched = (Project, Vec<Release>);

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
    /// The requested projects, in request order.
    requested: Vec<PackageId>,
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
            let release = self
                .records
                .get(&(root.package.clone(), wanted.to_owned()))
                .or_else(|| {
                    self.records.values().find(|(_, release)| {
                        release.project == root.package && release.number == wanted
                    })
                })
                .map(|(_, release)| release.id.clone());
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
        for kept in installed {
            let key = (kept.package.clone(), kept.release.clone());
            if !self.records.contains_key(&key) && self.kept.insert(key) {
                let supplies = overlay.entry(&kept.package).cloned().unwrap_or_default();
                self.candidates.push(Candidate {
                    package: kept.package.clone(),
                    release: kept.release.clone(),
                    order,
                    dependencies: Vec::new(),
                    provides: supplies.provides,
                    replaces: supplies.replaces,
                });
                order = order.saturating_add(1);
            }
            self.roots.push(RootRequirement {
                package: kept.package.clone(),
                release: Some(kept.release.clone()),
            });
        }
    }
}

/// Why resolution failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// An adapter request failed.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// The requirements cannot all be met.
    #[error(transparent)]
    Solver(#[from] SolverError),
    /// No permitted adapter serves a provider resolution needed.
    #[error("provider {provider:?} is unavailable: {reason}")]
    Unavailable {
        /// The provider id.
        provider: String,
        /// Why it is unavailable.
        reason: String,
    },
    /// A provider does not publish what resolution needs from it.
    #[error("provider {provider:?} does not publish {capability}")]
    Unsupported {
        /// The provider id.
        provider: String,
        /// What it does not publish.
        capability: &'static str,
    },
    /// A requested project does not support the target's game side.
    #[error("project {project:?} does not support the {side:?} target")]
    UnsupportedSide {
        /// The project's label.
        project: String,
        /// The target's side.
        side: Side,
    },
    /// A project has more releases than can be ranked.
    #[error("too many releases to rank for dependency resolution")]
    TooManyCandidates,
    /// The solver selected a release absent from the collected records.
    #[error("solver selected unavailable release {release:?} of {package}")]
    MissingCandidate {
        /// The project.
        package: PackageId,
        /// The release id.
        release: String,
    },
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io::Write};

    use msbe_core::solver::PackageId;
    use msbe_plan_schema::Side;

    use super::{
        Adapters, InstallPlan, InstalledRelease, Only, ProjectRequest, ResolveError, Resolver,
    };
    use crate::{
        Adapter, AdapterError, Availability, EndpointError, HttpClient, HttpError, HttpRequest,
        HttpResponse, Overlay, Releases, Target,
        model::{
            Channel, Dependency, DependencyKind, Download, Project, Release, ReleaseFile, Request,
        },
    };

    /// Fake adapters answer from memory, so no request ever reaches this.
    struct Offline;

    impl HttpClient for Offline {
        fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
            Err(offline(request.url))
        }

        fn download(&self, request: &HttpRequest<'_>, _: &mut dyn Write) -> Result<u64, HttpError> {
            Err(offline(request.url))
        }
    }

    fn offline(url: &str) -> HttpError {
        HttpError::Transport {
            url: url.to_owned(),
            message: "offline".to_owned(),
        }
    }

    fn not_found(what: &str) -> AdapterError {
        AdapterError::Endpoint(EndpointError::Http(HttpError::Status {
            url: what.to_owned(),
            status: 404,
        }))
    }

    fn package(provider: &str, project: &str) -> PackageId {
        PackageId {
            provider: provider.to_owned(),
            project: project.to_owned(),
        }
    }

    fn target() -> Target {
        Target {
            game: "game".to_owned(),
            edition: None,
            storefront: None,
            loader: "loader".to_owned(),
            provides: Vec::new(),
            loader_version: None,
            game_version: Some("1.0".to_owned()),
            side: Side::Client,
        }
    }

    /// A provider whose projects each have one release, held in memory.
    #[derive(Debug)]
    struct Memory {
        id: &'static str,
        releases: BTreeMap<String, Release>,
    }

    impl Memory {
        const fn new(id: &'static str) -> Self {
            Self {
                id,
                releases: BTreeMap::new(),
            }
        }

        fn publish(&mut self, project: &str, requires: &[PackageId]) {
            let release = Release {
                id: format!("{project}-1"),
                project: package(self.id, project),
                number: "1.0.0".to_owned(),
                channel: Channel::Release,
                published: "2026-09-01T00:00:00Z".to_owned(),
                files: vec![ReleaseFile {
                    download: Download::Direct {
                        url: format!("https://files.test/{project}.jar"),
                    },
                    name: format!("{project}.jar"),
                    size: None,
                    md5: None,
                    sha1: None,
                    sha256: None,
                    sha512: None,
                    primary: true,
                }],
                dependencies: requires
                    .iter()
                    .map(|required| Dependency {
                        project: Some(required.clone()),
                        release: None,
                        kind: DependencyKind::Required,
                    })
                    .collect(),
            };
            self.releases.insert(project.to_owned(), release);
        }
    }

    impl Adapter for Memory {
        fn id(&self) -> &str {
            self.id
        }

        fn request(&self, reference: &str) -> Result<Request, AdapterError> {
            Ok(Request::Project {
                reference: reference.to_owned(),
                version: None,
            })
        }

        fn as_releases(&self) -> Option<&dyn Releases> {
            Some(self)
        }
    }

    impl Releases for Memory {
        fn project(
            &self,
            _: &dyn HttpClient,
            reference: &str,
            _: &Target,
        ) -> Result<Project, AdapterError> {
            let release = self
                .releases
                .get(reference)
                .ok_or_else(|| not_found(reference))?;
            Ok(Project {
                id: release.project.clone(),
                slug: Some(reference.to_owned()),
                title: reference.to_owned(),
                client: Availability::Required,
                server: Availability::Required,
            })
        }

        fn releases(
            &self,
            _: &dyn HttpClient,
            project: &str,
            _: &Target,
        ) -> Result<Vec<Release>, AdapterError> {
            Ok(self.releases.get(project).cloned().into_iter().collect())
        }

        fn release_project(
            &self,
            _: &dyn HttpClient,
            release: &str,
            _: &Target,
        ) -> Result<PackageId, AdapterError> {
            Err(not_found(release))
        }
    }

    /// Two providers at once.
    struct Pair<'a>(&'a Memory, &'a Memory);

    impl Adapters for Pair<'_> {
        fn lookup(&self, provider: &str) -> Result<&dyn Adapter, ResolveError> {
            match [self.0, self.1]
                .into_iter()
                .find(|adapter| adapter.id == provider)
            {
                Some(adapter) => Ok(adapter),
                None => Err(ResolveError::Unavailable {
                    provider: provider.to_owned(),
                    reason: "not registered in this test".to_owned(),
                }),
            }
        }
    }

    fn labels(plan: &InstallPlan) -> Vec<&str> {
        plan.selections
            .iter()
            .map(|selection| selection.project.label())
            .collect()
    }

    #[test]
    fn a_project_from_another_provider_stands_in_when_the_overlay_says_so() {
        let mut alpha = Memory::new("alpha");
        alpha.publish("api", &[]);
        alpha.publish("shader", &[package("alpha", "api")]);
        let mut beta = Memory::new("beta");
        beta.publish("fork", &[]);
        // An entry naming a provider nobody registered must not fail resolution.
        let overlay = Overlay::from_toml(&[
            "schema = 1\nmod = \"beta:fork\"\nprovides = [\"alpha:api\"]",
            "schema = 1\nmod = \"gamma:gone\"\nprovides = [\"alpha:api\"]",
        ])
        .unwrap();
        let resolver = Resolver {
            adapters: &Pair(&alpha, &beta),
            http: &Offline,
            target: &target(),
            overlay: &overlay,
        };
        let shader = [ProjectRequest {
            provider: "alpha".to_owned(),
            reference: "shader".to_owned(),
            version: None,
        }];

        let fresh = resolver.plan_install(&shader, true, &[]).unwrap();
        assert_eq!(labels(&fresh), ["shader", "api"]);

        let forked = [InstalledRelease {
            package: package("beta", "fork"),
            release: "fork-1".to_owned(),
        }];
        let kept = resolver.plan_install(&shader, true, &forked).unwrap();
        assert_eq!(labels(&kept), ["shader", "fork"]);
        let [substitution] = kept.substitutions.as_slice() else {
            panic!("expected one substitution, got {:?}", kept.substitutions);
        };
        assert_eq!(substitution.requirement.package(), package("alpha", "api"));
        assert_eq!(substitution.supplied_by, package("beta", "fork"));
    }

    #[test]
    fn a_provider_that_publishes_no_releases_cannot_resolve_projects() {
        #[derive(Debug)]
        struct FilesOnly;

        impl Adapter for FilesOnly {
            fn id(&self) -> &'static str {
                "files"
            }

            fn request(&self, reference: &str) -> Result<Request, AdapterError> {
                Ok(Request::Project {
                    reference: reference.to_owned(),
                    version: None,
                })
            }
        }

        let overlay = Overlay::default();
        let resolver = Resolver {
            adapters: &Only(&FilesOnly),
            http: &Offline,
            target: &target(),
            overlay: &overlay,
        };
        let request = [ProjectRequest {
            provider: "files".to_owned(),
            reference: "anything".to_owned(),
            version: None,
        }];
        assert!(matches!(
            resolver.plan_install(&request, false, &[]),
            Err(ResolveError::Unsupported {
                capability: "project releases",
                ..
            })
        ));
    }
}
