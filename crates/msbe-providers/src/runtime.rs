//! Reviewed interpreters for the closed declarative provider-program vocabulary.
//!
//! `direct-url-v1` turns one pinned HTTPS URL into a file. `catalog-v1` serves search, projects,
//! releases, the project a release belongs to, and update checks from a JSON catalog, through only
//! the routes, parameters and JSON pointers a program declares (`docs/06-providers-and-policy.md`
//! §6.4). Everything else is fixed here and reviewed with MSBE: how target facts are encoded, how
//! releases are filtered and ordered, and which release may replace an installed one.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use msbe_provider_api::{
    Adapter, AdapterError, Availability, Capability, HttpClient, JsonEndpoint, PackageId,
    Provenance, ProviderProgram, Releases, RuntimeKind, Search, Target, Update, UpdateCheck,
    Updates,
    model::{
        Channel, Dependency, DependencyKind, Project, Release, ReleaseFile, Request, SearchResult,
        Selection,
    },
    program::{
        ChannelMapping, DependencyMapping, Facets, FileMapping, ObjectMapping, ReleaseOrder,
        ReleasesRequest, TargetFact, UpdateProtocol,
    },
};
use semver::Version;
use serde_json::{Map, Value};

/// The most bytes a catalog response may be.
const JSON_LIMIT: u64 = 16 << 20;
/// The longest project or release reference.
const REFERENCE_LIMIT: usize = 128;

/// Builds an adapter from a structurally validated provider program.
pub(super) fn build(program: ProviderProgram) -> Box<dyn Adapter> {
    Box::new(ProgramAdapter { program })
}

/// One adapter serves both runtimes; the program's runtime selects its behavior per operation.
#[derive(Debug)]
struct ProgramAdapter {
    program: ProviderProgram,
}

impl ProgramAdapter {
    fn has(&self, capability: Capability) -> bool {
        self.program.capabilities.contains(&capability)
    }

    fn catalog<'a>(&'a self, http: &'a dyn HttpClient) -> Result<Catalog<'a>, AdapterError> {
        let base = self
            .program
            .provider
            .api_base()
            .ok_or_else(|| specific(RuntimeError::MissingMetadata))?;
        Ok(Catalog {
            program: &self.program,
            endpoint: JsonEndpoint::new(http, base, JSON_LIMIT),
        })
    }

    /// `raw`, when it is a reference this runtime accepts and so safe as one route segment.
    fn reference<'r>(&self, raw: &'r str) -> Result<&'r str, AdapterError> {
        if is_reference(raw) {
            Ok(raw)
        } else {
            Err(self.invalid_reference(raw))
        }
    }

    fn invalid_reference(&self, raw: &str) -> AdapterError {
        specific(RuntimeError::InvalidReference {
            provider: self.program.provider.name.clone(),
            reference: raw.to_owned(),
        })
    }
}

impl Adapter for ProgramAdapter {
    fn id(&self) -> &str {
        &self.program.provider.id
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        match self.program.runtime {
            RuntimeKind::DirectUrlV1 => direct_selection(&self.program.provider.id, reference)
                .map(|selection| Request::File(Box::new(selection)))
                .map_err(specific),
            RuntimeKind::CatalogV1 => {
                let (project, version) = reference
                    .split_once('@')
                    .map_or((reference, None), |(project, version)| {
                        (project, Some(version))
                    });
                if !is_reference(project) || version.is_some_and(|version| !is_reference(version)) {
                    return Err(self.invalid_reference(reference));
                }
                Ok(Request::Project {
                    reference: project.to_owned(),
                    version: version.map(str::to_owned),
                })
            }
        }
    }

    fn as_search(&self) -> Option<&dyn Search> {
        self.has(Capability::Search).then_some(self)
    }

    fn as_releases(&self) -> Option<&dyn Releases> {
        (self.has(Capability::Project) && self.has(Capability::Releases)).then_some(self)
    }

    fn as_updates(&self) -> Option<&dyn Updates> {
        (self.has(Capability::Updates) && self.program.updates.is_some()).then_some(self)
    }
}

impl Search for ProgramAdapter {
    fn search(
        &self,
        http: &dyn HttpClient,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, AdapterError> {
        let catalog = self.catalog(http)?;
        let request = &self.program.search;
        let limit = request
            .maximum
            .map_or(limit, |maximum| limit.min(maximum))
            .to_string();
        let facets = request
            .facets
            .as_ref()
            .map(|facets| (facets.parameter.as_str(), facet_groups(facets, target)));
        let mut parameters = vec![(request.query.as_str(), query)];
        if let Some((parameter, groups)) = &facets {
            parameters.push((parameter, groups.as_str()));
        }
        parameters.push((request.limit.as_str(), limit.as_str()));
        let body = catalog.get(
            required_route(self.program.routes.search.as_deref())?,
            &parameters,
        )?;
        let mappings = &self.program.mappings;
        let hit = mappings.hit.as_ref().unwrap_or(&mappings.project);
        let mut results = Vec::new();
        for item in required_items(&body, mappings.search_items.as_deref())? {
            let client = availability(item, hit.client.as_deref());
            let server = availability(item, hit.server.as_deref());
            if target.supports_side(client, server) {
                results.push(catalog.search_result(item, hit)?);
            }
        }
        Ok(results)
    }
}

impl Releases for ProgramAdapter {
    fn project(&self, http: &dyn HttpClient, reference: &str) -> Result<Project, AdapterError> {
        let reference = self.reference(reference)?;
        let catalog = self.catalog(http)?;
        let route = interpolate(
            required_route(self.program.routes.project.as_deref())?,
            "reference",
            reference,
        )?;
        catalog.project(&catalog.get(&route, &[])?)
    }

    fn releases(
        &self,
        http: &dyn HttpClient,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Release>, AdapterError> {
        let project = self.reference(project)?;
        let catalog = self.catalog(http)?;
        let route = interpolate_game(
            required_route(self.program.routes.releases.as_deref())?,
            &self.program.games,
            &target.game,
        )?;
        let route = interpolate(&route, "project", project)?;
        let parameters = release_parameters(&self.program.releases, target);
        let parameters: Vec<(&str, &str)> = parameters
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let body = catalog.get(&route, &parameters)?;
        let listed = body
            .as_array()
            .ok_or_else(|| specific(RuntimeError::ExpectedArray))?;
        let mut releases = Vec::new();
        for value in listed {
            if catalog.supports(value, target)? {
                releases.push(catalog.release(value, Some(project))?);
            }
        }
        match self.program.releases.order {
            ReleaseOrder::NewestFirst => releases.sort_by(|a, b| b.published.cmp(&a.published)),
            ReleaseOrder::Semver => releases.sort_by(|a, b| {
                Version::parse(&b.number)
                    .ok()
                    .cmp(&Version::parse(&a.number).ok())
                    .then_with(|| b.published.cmp(&a.published))
            }),
            ReleaseOrder::Listed => {}
        }
        Ok(releases)
    }

    fn release_project(
        &self,
        http: &dyn HttpClient,
        release: &str,
    ) -> Result<PackageId, AdapterError> {
        if !self.has(Capability::ReleaseProject) {
            return Err(specific(RuntimeError::UnsupportedReleaseProject));
        }
        let release = self.reference(release)?;
        let catalog = self.catalog(http)?;
        let route = interpolate(
            required_route(self.program.routes.release.as_deref())?,
            "release",
            release,
        )?;
        let body = catalog.get(&route, &[])?;
        Ok(catalog.package(required_text(
            &body,
            self.program.mappings.release.project.as_deref(),
        )?))
    }
}

impl Updates for ProgramAdapter {
    fn check(
        &self,
        http: &dyn HttpClient,
        installed: &[&Provenance],
        target: &Target,
    ) -> Result<Vec<UpdateCheck>, AdapterError> {
        let protocol = self
            .program
            .updates
            .as_ref()
            .ok_or_else(|| specific(RuntimeError::NoUpdateProtocol))?;
        let catalog = self.catalog(http)?;
        match protocol.kind {
            msbe_provider_api::program::UpdateProtocolKind::HashLookupV1 => {
                catalog.check_updates(protocol, installed, target)
            }
            msbe_provider_api::program::UpdateProtocolKind::ReleasesV1 => {
                catalog.check_release_updates(installed, target)
            }
        }
    }
}

/// One catalog program's view of its API, for one operation.
struct Catalog<'a> {
    program: &'a ProviderProgram,
    endpoint: JsonEndpoint<'a>,
}

impl Catalog<'_> {
    fn get(&self, route: &str, query: &[(&str, &str)]) -> Result<Value, AdapterError> {
        Ok(self.endpoint.get(route, query)?)
    }

    fn post(
        &self,
        route: &str,
        body: Map<String, Value>,
    ) -> Result<BTreeMap<String, Value>, AdapterError> {
        let answer: BTreeMap<String, Value> = self.endpoint.post(route, &Value::Object(body))?;
        Ok(answer
            .into_iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value))
            .collect())
    }

    fn package(&self, project: String) -> PackageId {
        PackageId {
            provider: self.program.provider.id.clone(),
            project,
        }
    }

    fn project(&self, value: &Value) -> Result<Project, AdapterError> {
        let map = &self.program.mappings.project;
        Ok(Project {
            id: self.package(required_text(value, map.id.as_deref())?),
            slug: map
                .slug
                .as_deref()
                .map(|pointer| required_text(value, Some(pointer)))
                .transpose()?,
            title: required_text(value, map.title.as_deref())?,
            client: availability(value, map.client.as_deref()),
            server: availability(value, map.server.as_deref()),
        })
    }

    fn search_result(
        &self,
        value: &Value,
        map: &ObjectMapping,
    ) -> Result<SearchResult, AdapterError> {
        Ok(SearchResult {
            provider: self.program.provider.id.clone(),
            project: required_text(value, map.id.as_deref())?,
            reference: required_text(value, map.slug.as_deref().or(map.id.as_deref()))?,
            title: required_text(value, map.title.as_deref())?,
            description: optional_text(value, map.description.as_deref()).unwrap_or_default(),
            icon_url: optional_text(value, map.icon.as_deref()),
            downloads: map
                .downloads
                .as_deref()
                .and_then(|pointer| value.pointer(pointer))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        })
    }

    /// Whether the release in `value` supports the target's game version, loaders, and loader
    /// version. A release that declares no versions for the target's loaders accepts any.
    fn supports(&self, value: &Value, target: &Target) -> Result<bool, AdapterError> {
        let map = &self.program.mappings.release;
        let game_versions = strings(value, map.game_versions.as_deref())?;
        let loaders = strings(value, map.loaders.as_deref())?;
        let loader_ids: Vec<&str> = target.loader_ids().collect();
        let declared = map
            .loader_versions
            .as_deref()
            .and_then(|pointer| value.pointer(pointer))
            .and_then(Value::as_object);
        let version_matches = match (target.loader_version.as_deref(), declared) {
            (Some(wanted), Some(declared)) => {
                let versions: Vec<&str> = loader_ids
                    .iter()
                    .filter_map(|loader| declared.get(*loader))
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                versions.is_empty() || versions.contains(&wanted)
            }
            _ => true,
        };
        Ok(map.game_versions.is_none()
            || target.game_version.as_ref().is_none_or(|wanted| {
                game_versions.is_empty() || game_versions.iter().any(|version| version == wanted)
            }) && (map.loaders.is_none()
                || loaders
                    .iter()
                    .any(|loader| loader_ids.contains(&loader.as_str())))
                && version_matches)
    }

    /// The release in `value`, which belongs to `listed` when the program does not map a release's
    /// project.
    fn release(&self, value: &Value, listed: Option<&str>) -> Result<Release, AdapterError> {
        let map = &self.program.mappings.release;
        let project = match map.project.as_deref() {
            Some(pointer) => required_text(value, Some(pointer))?,
            None => listed
                .map(str::to_owned)
                .ok_or_else(|| specific(RuntimeError::MissingMapping))?,
        };
        let files = required_items(value, map.files.as_deref())
            .or_else(|error| absent_as_empty(value, map.files.as_deref(), error))?
            .iter()
            .map(|file_value| file(file_value, &map.file))
            .collect::<Result<_, _>>()?;
        let dependencies = required_items(value, map.dependencies.as_deref())
            .or_else(|error| absent_as_empty(value, map.dependencies.as_deref(), error))?
            .iter()
            .map(|dependency| self.dependency(dependency, &map.dependency))
            .collect();
        Ok(Release {
            id: required_text(value, map.id.as_deref())?,
            project: self.package(project),
            number: required_text(value, map.number.as_deref())?,
            channel: channel(value, map.channel.as_ref()),
            published: required_text(value, map.published.as_deref())?,
            files,
            dependencies,
        })
    }

    fn dependency(&self, value: &Value, mapping: &DependencyMapping) -> Dependency {
        let kind = match optional_text(value, mapping.kind.as_deref()).as_deref() {
            Some("required") => DependencyKind::Required,
            Some("optional") => DependencyKind::Optional,
            Some("incompatible") => DependencyKind::Incompatible,
            Some("embedded") => DependencyKind::Embedded,
            _ => DependencyKind::Unknown,
        };
        Dependency {
            project: optional_text(value, mapping.project.as_deref())
                .map(|project| self.package(project)),
            release: optional_text(value, mapping.release.as_deref()),
            kind,
        }
    }

    /// Checks installed files for releases to replace them with, answering in the order
    /// `installed` is given, under the `hash-lookup-v1` protocol.
    ///
    /// A file stays on its release channel or moves to a more stable one. A replacement is always
    /// newer than the installed release, unless the installed release does not support the target;
    /// then the newest compatible release on its channel is offered, even if it is older. Costs
    /// one request for the installed releases and one per channel in use.
    fn check_updates(
        &self,
        protocol: &UpdateProtocol,
        installed: &[&Provenance],
        target: &Target,
    ) -> Result<Vec<UpdateCheck>, AdapterError> {
        let algorithm = protocol
            .algorithm
            .ok_or_else(|| specific(RuntimeError::InvalidUpdateProtocol))?
            .as_str();
        let listed_route = protocol
            .listed
            .as_deref()
            .ok_or_else(|| specific(RuntimeError::InvalidUpdateProtocol))?;
        let latest_route = protocol
            .latest
            .as_deref()
            .ok_or_else(|| specific(RuntimeError::InvalidUpdateProtocol))?;
        let fields = protocol
            .fields
            .as_ref()
            .ok_or_else(|| specific(RuntimeError::InvalidUpdateProtocol))?;
        let requested: Vec<String> = installed
            .iter()
            .filter_map(|provenance| provenance.hashes.get(algorithm))
            .map(|hash| hash.to_ascii_lowercase())
            .collect();
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let unique: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
        let listed = self.post(
            listed_route,
            Map::from_iter([
                (
                    fields.hashes.clone(),
                    Value::from(unique.into_iter().collect::<Vec<_>>()),
                ),
                (fields.algorithm.clone(), Value::from(algorithm)),
            ]),
        )?;

        let names = self
            .program
            .mappings
            .release
            .channel
            .as_ref()
            .ok_or_else(|| specific(RuntimeError::MissingMapping))?;
        let mut groups: BTreeMap<Vec<&str>, Vec<&str>> = BTreeMap::new();
        for (hash, release) in &listed {
            groups
                .entry(allowed_channels(channel(release, Some(names)), names))
                .or_default()
                .push(hash);
        }
        let loaders: Vec<&str> = target.loader_ids().collect();
        let mut latest = BTreeMap::new();
        for (channels, hashes) in groups {
            latest.extend(
                self.post(
                    latest_route,
                    Map::from_iter([
                        (fields.hashes.clone(), Value::from(hashes)),
                        (fields.algorithm.clone(), Value::from(algorithm)),
                        (fields.loaders.clone(), Value::from(loaders.clone())),
                        (
                            fields.game_versions.clone(),
                            Value::from(
                                target
                                    .game_version
                                    .iter()
                                    .map(String::as_str)
                                    .collect::<Vec<_>>(),
                            ),
                        ),
                        (fields.channels.clone(), Value::from(channels)),
                    ]),
                )?,
            );
        }

        requested
            .iter()
            .map(|hash| match listed.get(hash) {
                Some(release) => self.decide(release, latest.get(hash), target),
                None => Ok(UpdateCheck::Unlisted),
            })
            .collect()
    }

    fn check_release_updates(
        &self,
        installed: &[&Provenance],
        target: &Target,
    ) -> Result<Vec<UpdateCheck>, AdapterError> {
        installed
            .iter()
            .map(|provenance| {
                let route = interpolate_game(
                    required_route(self.program.routes.releases.as_deref())?,
                    &self.program.games,
                    &target.game,
                )?;
                let route = interpolate(&route, "project", &provenance.project)?;
                let body = self.get(&route, &[])?;
                let listed = body
                    .as_array()
                    .ok_or_else(|| specific(RuntimeError::ExpectedArray))?;
                let mut releases: Vec<Release> = listed
                    .iter()
                    .filter(|value| self.supports(value, target).unwrap_or(false))
                    .filter_map(|value| self.release(value, Some(&provenance.project)).ok())
                    .collect();
                releases.sort_by(|a, b| {
                    Version::parse(&b.number)
                        .ok()
                        .cmp(&Version::parse(&a.number).ok())
                });
                let Some(latest) = releases.first() else {
                    return Ok(UpdateCheck::Unlisted);
                };
                if latest.number == provenance.version_number {
                    return Ok(UpdateCheck::Current);
                }
                let file = latest
                    .primary_file()
                    .cloned()
                    .ok_or_else(|| AdapterError::NoFiles {
                        project: latest.project.clone(),
                        release: latest.id.clone(),
                    })?;
                Ok(UpdateCheck::Available(Box::new(Update {
                    release: latest.clone(),
                    file,
                })))
            })
            .collect()
    }

    /// Whether `latest`, the newest release on the installed release's channel, should replace it.
    fn decide(
        &self,
        installed: &Value,
        latest: Option<&Value>,
        target: &Target,
    ) -> Result<UpdateCheck, AdapterError> {
        let map = &self.program.mappings.release;
        let fits = self.supports(installed, target)?;
        let mut replacement = None;
        if let Some(candidate) = latest
            && required_text(candidate, map.id.as_deref())?
                != required_text(installed, map.id.as_deref())?
            && self.supports(candidate, target)?
            && (!fits
                || required_text(candidate, map.published.as_deref())?
                    > required_text(installed, map.published.as_deref())?)
        {
            replacement = Some(candidate);
        }
        Ok(match replacement {
            Some(candidate) => {
                let release = self.release(candidate, None)?;
                let file =
                    release
                        .primary_file()
                        .cloned()
                        .ok_or_else(|| AdapterError::NoFiles {
                            project: release.project.clone(),
                            release: release.id.clone(),
                        })?;
                UpdateCheck::Available(Box::new(Update { release, file }))
            }
            None if fits => UpdateCheck::Current,
            None => UpdateCheck::Incompatible,
        })
    }
}

/// The facet groups for `target`, as a JSON array of arrays of strings.
fn facet_groups(facets: &Facets, target: &Target) -> String {
    let groups: Vec<Vec<String>> = facets
        .groups
        .iter()
        .map(|group| {
            group
                .iter()
                .flat_map(|template| {
                    if template.contains("{game}") {
                        vec![template.replace("{game}", &target.game)]
                    } else if template.contains("{loader}") {
                        target
                            .loader_ids()
                            .map(|loader| template.replace("{loader}", loader))
                            .collect()
                    } else if let Some(game_version) = &target.game_version {
                        vec![template.replace("{game_version}", game_version)]
                    } else {
                        Vec::new()
                    }
                })
                .collect::<Vec<_>>()
        })
        .filter(|group| !group.is_empty())
        .collect();
    serde_json::to_string(&groups).unwrap_or_default()
}

/// A release listing's query parameters for `target`.
fn release_parameters(request: &ReleasesRequest, target: &Target) -> Vec<(String, String)> {
    request
        .query
        .iter()
        .filter_map(|parameter| {
            let values: Vec<&str> = match parameter.target {
                Some(TargetFact::Game) => vec![target.game.as_str()],
                Some(TargetFact::Loaders) => target.loader_ids().collect(),
                Some(TargetFact::GameVersion) => {
                    target.game_version.iter().map(String::as_str).collect()
                }
                None => Vec::new(),
            };
            if parameter.target.is_some() && values.is_empty() {
                return None;
            }
            let values: Vec<&str> = values
                .into_iter()
                .filter_map(|value| {
                    parameter
                        .values
                        .get(value)
                        .map_or(Some(value), |mapped| Some(mapped.as_str()))
                })
                .collect();
            if parameter.target.is_some() && values.is_empty() {
                return None;
            }
            let value = parameter
                .literal
                .clone()
                .unwrap_or_else(|| serde_json::to_string(&values).unwrap_or_default());
            Some((parameter.name.clone(), value))
        })
        .collect()
}

/// The channels a release on `installed` may move to, in the catalog's names: its own, or more
/// stable.
fn allowed_channels(installed: Channel, names: &ChannelMapping) -> Vec<&str> {
    match installed {
        Channel::Release => vec![names.release.as_str()],
        Channel::Beta => vec![names.release.as_str(), names.beta.as_str()],
        Channel::Alpha | Channel::Unknown => vec![
            names.release.as_str(),
            names.beta.as_str(),
            names.alpha.as_str(),
        ],
    }
}

/// The channel of the release in `value`; unknown when unmapped or unnamed.
fn channel(value: &Value, names: Option<&ChannelMapping>) -> Channel {
    let Some(names) = names else {
        return Channel::Unknown;
    };
    match value.pointer(&names.pointer).and_then(Value::as_str) {
        Some(name) if name == names.release => Channel::Release,
        Some(name) if name == names.beta => Channel::Beta,
        Some(name) if name == names.alpha => Channel::Alpha,
        _ => Channel::Unknown,
    }
}

/// A side's availability: optional when unmapped, unknown when mapped but absent or unrecognized.
fn availability(value: &Value, pointer: Option<&str>) -> Availability {
    let Some(pointer) = pointer else {
        return Availability::Optional;
    };
    value
        .pointer(pointer)
        .cloned()
        .and_then(|found| serde_json::from_value(found).ok())
        .unwrap_or_default()
}

fn required_route(route: Option<&str>) -> Result<&str, AdapterError> {
    route.ok_or_else(|| specific(RuntimeError::MissingRoute))
}

fn required_text(value: &Value, pointer: Option<&str>) -> Result<String, AdapterError> {
    let pointer = pointer.ok_or_else(|| specific(RuntimeError::MissingMapping))?;
    value
        .pointer(pointer)
        .ok_or_else(|| specific(RuntimeError::MissingPointer(pointer.to_owned())))?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| specific(RuntimeError::ExpectedText))
}

/// Text at `pointer`, or `None` when it is unmapped, absent, or not text.
fn optional_text(value: &Value, pointer: Option<&str>) -> Option<String> {
    pointer
        .and_then(|pointer| value.pointer(pointer))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn required_items<'v>(
    value: &'v Value,
    pointer: Option<&str>,
) -> Result<&'v [Value], AdapterError> {
    let pointer = pointer.ok_or_else(|| specific(RuntimeError::MissingMapping))?;
    value
        .pointer(pointer)
        .ok_or_else(|| specific(RuntimeError::MissingPointer(pointer.to_owned())))?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| specific(RuntimeError::ExpectedArray))
}

/// No items when the mapped array is absent or null; otherwise `error`.
fn absent_as_empty<'v>(
    value: &'v Value,
    pointer: Option<&str>,
    error: AdapterError,
) -> Result<&'v [Value], AdapterError> {
    match pointer.map(|pointer| value.pointer(pointer)) {
        Some(None | Some(Value::Null)) => Ok(&[]),
        _ => Err(error),
    }
}

/// The strings in the mapped array; none when it is absent or null.
fn strings(value: &Value, pointer: Option<&str>) -> Result<Vec<String>, AdapterError> {
    required_items(value, pointer)
        .or_else(|error| absent_as_empty(value, pointer, error))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| specific(RuntimeError::ExpectedText))
        })
        .collect()
}

fn file(value: &Value, mapping: &FileMapping) -> Result<ReleaseFile, AdapterError> {
    Ok(ReleaseFile {
        url: required_text(value, mapping.url.as_deref())?,
        name: required_text(value, mapping.name.as_deref())?,
        size: mapping
            .size
            .as_deref()
            .and_then(|pointer| value.pointer(pointer))
            .and_then(Value::as_u64),
        sha256: optional_text(value, mapping.sha256.as_deref()),
        sha512: optional_text(value, mapping.sha512.as_deref()),
        primary: mapping.primary.as_deref().is_none_or(|pointer| {
            value
                .pointer(pointer)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }),
    })
}

/// Whether `raw` can be a slug, id or version number, and is safe as one route segment.
fn is_reference(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= REFERENCE_LIMIT
        && raw
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
        && raw.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+')
        })
}

fn interpolate(route: &str, name: &str, value: &str) -> Result<String, AdapterError> {
    let marker = format!("{{{name}}}");
    if route.matches(&marker).count() == 1 {
        Ok(route.replace(&marker, value))
    } else {
        Err(specific(RuntimeError::InvalidRouteTemplate))
    }
}

fn interpolate_game(
    route: &str,
    games: &BTreeMap<String, String>,
    game: &str,
) -> Result<String, AdapterError> {
    if !route.contains("{game}") {
        return Ok(route.to_owned());
    }
    let value = games
        .get(game)
        .ok_or_else(|| specific(RuntimeError::UnsupportedGame(game.to_owned())))?;
    interpolate(route, "game", value)
}

fn specific(error: RuntimeError) -> AdapterError {
    AdapterError::specific(error)
}

fn direct_selection(provider: &str, raw: &str) -> Result<Selection, RuntimeError> {
    let (url, checksum) = raw
        .split_once('#')
        .map_or((raw, None), |(url, fragment)| (url, Some(fragment)));
    let path = url
        .strip_prefix("https://")
        .ok_or(RuntimeError::InsecureUrl)?
        .split('?')
        .next()
        .unwrap_or_default();
    let name = path
        .split_once('/')
        .and_then(|(host, path)| (!host.is_empty()).then_some(path))
        .and_then(|path| path.rsplit('/').next())
        .and_then(percent_decode)
        .filter(|name| safe_file_name(name))
        .ok_or(RuntimeError::InvalidUrl)?;
    let (sha256, sha512) =
        checksum
            .map(parse_checksum)
            .transpose()?
            .map_or((None, None), |(algorithm, digest)| {
                if algorithm == "sha256" {
                    (Some(digest), None)
                } else {
                    (None, Some(digest))
                }
            });
    let package = PackageId {
        provider: provider.to_owned(),
        project: url.to_owned(),
    };
    let file = ReleaseFile {
        url: url.to_owned(),
        name: name.clone(),
        size: None,
        sha256,
        sha512,
        primary: true,
    };
    Ok(Selection {
        project: Project {
            id: package.clone(),
            slug: None,
            title: name.clone(),
            client: Availability::Optional,
            server: Availability::Optional,
        },
        release: Release {
            id: url.to_owned(),
            project: package,
            number: name,
            channel: Channel::Unknown,
            published: String::new(),
            files: vec![file.clone()],
            dependencies: Vec::new(),
        },
        file,
        required_by: None,
    })
}

fn percent_decode(raw: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let escape = bytes
                .get(index + 1..index + 3)
                .filter(|escape| escape.iter().all(u8::is_ascii_hexdigit))?;
            decoded.push(u8::from_str_radix(std::str::from_utf8(escape).ok()?, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn safe_file_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '\0'])
}

fn parse_checksum(fragment: &str) -> Result<(&str, String), RuntimeError> {
    let (algorithm, digest) = fragment
        .split_once('=')
        .ok_or(RuntimeError::InvalidChecksum)?;
    let length = if algorithm == "sha256" {
        64
    } else if algorithm == "sha512" {
        128
    } else {
        return Err(RuntimeError::InvalidChecksum);
    };
    if digest.len() == length
        && digest
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        Ok((algorithm, digest.to_ascii_lowercase()))
    } else {
        Err(RuntimeError::InvalidChecksum)
    }
}

#[derive(Debug)]
pub(super) enum RuntimeError {
    MissingMetadata,
    MissingRoute,
    MissingMapping,
    MissingPointer(String),
    ExpectedArray,
    ExpectedText,
    InvalidReference { provider: String, reference: String },
    InvalidRouteTemplate,
    UnsupportedReleaseProject,
    NoUpdateProtocol,
    InsecureUrl,
    InvalidUrl,
    InvalidChecksum,
    InvalidUpdateProtocol,
    UnsupportedGame(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingPointer(pointer) => {
                return write!(
                    formatter,
                    "provider program JSON pointer {pointer:?} was absent"
                );
            }
            Self::InvalidReference {
                provider,
                reference,
            } => {
                return write!(
                    formatter,
                    "{reference:?} is not a valid {provider} project or version reference"
                );
            }
            Self::MissingMetadata => "provider program is missing required metadata",
            Self::MissingRoute => "provider program has no route for this operation",
            Self::MissingMapping => "provider program has no mapping for a required record field",
            Self::ExpectedArray => "provider program expected a JSON array in the response",
            Self::ExpectedText => "provider program expected a JSON string in the response",
            Self::InvalidRouteTemplate => "provider program route template is invalid",
            Self::UnsupportedReleaseProject => {
                "provider program cannot resolve a release for this project"
            }
            Self::NoUpdateProtocol => "provider program declares no update protocol",
            Self::InsecureUrl => "provider program refused an insecure URL; only https is allowed",
            Self::InvalidUrl => "provider program produced an invalid URL",
            Self::InvalidChecksum => "provider program found an invalid checksum",
            Self::InvalidUpdateProtocol => "provider program has an invalid update protocol",
            Self::UnsupportedGame(game) => {
                return write!(formatter, "provider program does not support game {game:?}");
            }
        };
        formatter.write_str(message)
    }
}

impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::direct_selection;

    #[test]
    fn direct_urls_decode_safe_percent_escaped_file_names() {
        let selection = direct_selection("url", "https://example.test/mod%20file.jar").unwrap();
        assert_eq!(selection.file.name, "mod file.jar");
        for url in [
            "https://example.test",
            "https://example.test/mod%",
            "https://example.test/mod%2",
            "https://example.test/mod%2fother.jar",
            "https://example.test/%ff.jar",
        ] {
            assert!(direct_selection("url", url).is_err(), "{url}");
        }
    }
}
