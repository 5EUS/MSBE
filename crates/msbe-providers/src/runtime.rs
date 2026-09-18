//! Reviewed interpreters for the closed declarative provider-program vocabulary.
//!
//! `direct-url-v1` turns one pinned HTTPS URL into a file. `tool-v1` resolves an item to one release
//! whose file the tool the user registered fetches, through the host its caller supplies. `catalog-v1` serves search, projects,
//! releases, the project a release belongs to, and update checks from a JSON catalog, through only
//! the games, translations, routes, pages, parameters and JSON pointers a program declares
//! (`docs/06-providers-and-policy.md` §6.4). Everything else is fixed here and reviewed with MSBE:
//! how target facts are translated and encoded, how releases are filtered and ordered, how a file
//! can be obtained, and which release may replace an installed one.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::Path,
};

use msbe_provider_api::{
    Accounts, AcquiredArtifact, Adapter, AdapterError, ApiHeaders, Availability, Capability,
    Handoff, HttpClient, JsonEndpoint, PackageId, Provenance, ProviderProgram, Redeemed, Releases,
    RuntimeKind, Search, Target, ToolError, ToolHost, Update, UpdateCheck, Updates,
    acquire_download, acquired_directory, is_tool_value,
    manifest::Acquisition,
    model::{
        Account, ActionReason, Channel, Dependency, DependencyKind, Download, HandoffTicket,
        Project, Release, ReleaseFile, Request, SearchResult, Selection,
    },
    program::{
        ChannelMapping, Condition, DependencyMapping, EachSelector, Encoding, Facets,
        FilteredItems, HandoffQuery, HashAlgorithm, Items, ObjectMapping, QueryParameter,
        ReleaseOrder, Selector, TargetFact, UpdateFields, UpdateProtocol,
    },
};
use semver::Version;
use serde_json::{Map, Value};

/// The most bytes a catalog response may be.
const JSON_LIMIT: u64 = 16 << 20;
/// The longest project or release reference.
const REFERENCE_LIMIT: usize = 128;
/// The longest handoff link read.
const LINK_LIMIT: usize = 2048;
/// The id and number of the one release a `tool-v1` item has: whatever the tool fetches now.
const TOOL_RELEASE: &str = "tool";

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
    /// The tool's identifier for `target`'s game, for a `tool-v1` program.
    fn tool_game(&self, target: &Target) -> Result<&str, AdapterError> {
        self.program
            .game_id(&target.game, target.edition.as_deref())
            .ok_or_else(|| {
                specific(RuntimeError::UnsupportedGame {
                    provider: self.program.provider.name.clone(),
                    game: target.game.clone(),
                    edition: target.edition.clone(),
                })
            })
    }

    fn tool_package(&self, item: &str) -> Result<PackageId, AdapterError> {
        if is_tool_value(item) {
            Ok(PackageId {
                provider: self.program.provider.id.clone(),
                project: item.to_owned(),
            })
        } else {
            Err(invalid_reference(&self.program, item))
        }
    }

    fn has(&self, capability: Capability) -> bool {
        self.program.capabilities.contains(&capability)
    }

    /// The catalog, as seen for `target`. Refuses a game, or edition of one, the program does not
    /// serve before any request is made.
    fn catalog<'a>(
        &'a self,
        http: &'a dyn HttpClient,
        target: &'a Target,
    ) -> Result<Catalog<'a>, AdapterError> {
        let base = self
            .program
            .provider
            .api_base()
            .ok_or_else(|| specific(RuntimeError::MissingMetadata))?;
        let game = self
            .program
            .game_id(&target.game, target.edition.as_deref())
            .ok_or_else(|| {
                specific(RuntimeError::UnsupportedGame {
                    provider: self.program.provider.name.clone(),
                    game: target.game.clone(),
                    edition: target.edition.clone(),
                })
            })?;
        Ok(Catalog {
            program: &self.program,
            endpoint: JsonEndpoint::new(http, base, JSON_LIMIT),
            target: Some(target),
            game,
        })
    }

    /// The catalog a handoff link is read against: the link's own game, and no instance target.
    fn link_catalog<'a>(
        &'a self,
        http: &'a dyn HttpClient,
        ticket: &'a HandoffTicket,
    ) -> Result<Catalog<'a>, AdapterError> {
        let base = self
            .program
            .provider
            .api_base()
            .ok_or_else(|| specific(RuntimeError::MissingMetadata))?;
        Ok(Catalog {
            program: &self.program,
            endpoint: JsonEndpoint::new(http, base, JSON_LIMIT),
            target: None,
            game: &ticket.catalog_game,
        })
    }
}

impl Adapter for ProgramAdapter {
    fn id(&self) -> &str {
        &self.program.provider.id
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        match self.program.runtime {
            RuntimeKind::ToolV1 => {
                self.tool_package(reference)?;
                Ok(Request::Project {
                    reference: reference.to_owned(),
                    version: None,
                })
            }
            RuntimeKind::DirectUrlV1 => direct_selection(&self.program.provider.id, reference)
                .map(|selection| Request::File(Box::new(selection)))
                .map_err(specific),
            RuntimeKind::CatalogV1 => {
                let (project, version) = reference
                    .split_once('@')
                    .map_or((reference, None), |(project, version)| {
                        (project, Some(version))
                    });
                if route_segments(&self.program, project).is_none()
                    || version.is_some_and(|version| !is_reference(version))
                {
                    return Err(invalid_reference(&self.program, reference));
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
        (self.program.runtime == RuntimeKind::ToolV1
            || (self.has(Capability::Project) && self.has(Capability::Releases)))
        .then_some(self)
    }

    fn as_updates(&self) -> Option<&dyn Updates> {
        (self.has(Capability::Updates) && self.program.updates.is_some()).then_some(self)
    }

    fn as_handoff(&self) -> Option<&dyn Handoff> {
        (self.program.handoff.is_some() && self.program.provider.handoff_scheme().is_some())
            .then_some(self)
    }

    fn as_accounts(&self) -> Option<&dyn Accounts> {
        (self.program.auth.is_some() && self.program.mappings.account.is_some()).then_some(self)
    }

    /// The credential header `[auth]` names and the quota headers `[rate_limit]` names.
    fn api_headers(&self) -> ApiHeaders {
        ApiHeaders {
            credential: self.program.auth.as_ref().map(|auth| auth.header.clone()),
            quota: self
                .program
                .rate_limit
                .as_ref()
                .map(|rate| rate.remaining.clone())
                .unwrap_or_default(),
        }
    }

    /// A file a registered tool fetches is fetched through `tools`, into a fresh directory beneath
    /// `dir`; any other file is downloaded as every adapter downloads it. The tool succeeded only if
    /// it left something where `[tool] output` says the item lands.
    fn acquire(
        &self,
        http: &dyn HttpClient,
        tools: &dyn ToolHost,
        file: &ReleaseFile,
        dir: &Path,
    ) -> Result<AcquiredArtifact, AdapterError> {
        let Download::Tool { game, item } = &file.download else {
            return acquire_download(http, file, dir);
        };
        let tool = self
            .program
            .tool
            .as_ref()
            .filter(|_| self.program.runtime == RuntimeKind::ToolV1)
            .ok_or_else(|| specific(RuntimeError::NoTool))?;
        if !is_tool_value(game) || self.program.game_with_id(game).is_none() {
            return Err(invalid_reference(&self.program, game));
        }
        self.tool_package(item)?;
        let invocation = tool.invocation(&self.program.provider.id, game, item);
        let output = tools.run(&invocation, dir)?;
        let fetched = invocation
            .output
            .iter()
            .fold(output, |path, segment| path.join(segment));
        let provider = || self.program.provider.id.clone();
        let empty = fs::read_dir(&fetched).map_or(true, |mut entries| entries.next().is_none());
        if empty {
            return Err(ToolError::Empty {
                tool: provider(),
                path: fetched.display().to_string(),
            }
            .into());
        }
        acquired_directory(&fetched).map_err(|error| {
            ToolError::Host {
                tool: provider(),
                message: format!("cannot read what it fetched: {error}"),
            }
            .into()
        })
    }

    /// Records the strong digests, plus the one the update protocol looks files up by, so a file
    /// installed from a catalog that publishes only a weak digest can still be found again.
    fn provenance(&self, release: &Release, acquired: &AcquiredArtifact) -> Provenance {
        let mut hashes = BTreeMap::from([
            ("sha256".to_owned(), acquired.sha256.clone()),
            ("sha512".to_owned(), acquired.sha512.clone()),
        ]);
        if let Some(algorithm) = self
            .program
            .updates
            .as_ref()
            .and_then(UpdateProtocol::algorithm)
            && let Some(digest) = acquired.digest(algorithm.as_str())
        {
            hashes.insert(algorithm.as_str().to_owned(), digest.to_owned());
        }
        Provenance {
            provider: release.project.provider.clone(),
            project: release.project.project.clone(),
            version: release.id.clone(),
            version_number: release.number.clone(),
            hashes,
        }
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
        let catalog = self.catalog(http, target)?;
        let request = &self.program.search;
        let limit = request
            .maximum
            .map_or(limit, |maximum| limit.min(maximum))
            .to_string();
        let facets = request
            .facets
            .as_ref()
            .map(|facets| (facets.parameter.as_str(), catalog.facet_groups(facets)));
        let filters = catalog.parameters(&request.parameters);
        let mut parameters = vec![(request.query.as_str(), query)];
        if let Some((parameter, groups)) = &facets {
            parameters.push((parameter, groups.as_str()));
        }
        parameters.extend(
            filters
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        parameters.push((request.limit.as_str(), limit.as_str()));
        let route = catalog.route(required_route(self.program.routes.search.as_deref())?, None)?;
        let body = catalog.get(&route, &parameters)?;
        let mappings = &self.program.mappings;
        let hit = mappings.hit.as_ref().unwrap_or(&mappings.project);
        let mut results = Vec::new();
        for item in required_items(&body, mappings.search_items.as_deref())? {
            let client = availability(item, hit.client.as_deref());
            let server = availability(item, hit.server.as_deref());
            if target.supports_side(client, server) && catalog.listed_for_game(item, hit)? {
                results.push(catalog.search_result(item, hit)?);
            }
        }
        Ok(results)
    }
}

impl Releases for ProgramAdapter {
    fn project(
        &self,
        http: &dyn HttpClient,
        reference: &str,
        target: &Target,
    ) -> Result<Project, AdapterError> {
        if self.program.runtime == RuntimeKind::ToolV1 {
            self.tool_game(target)?;
            return Ok(Project {
                id: self.tool_package(reference)?,
                slug: Some(reference.to_owned()),
                title: reference.to_owned(),
                client: Availability::Optional,
                server: Availability::Optional,
            });
        }
        let catalog = self.catalog(http, target)?;
        let segments = catalog.segments(reference)?;
        let route = catalog.route(
            required_route(self.program.routes.project.as_deref())?,
            Some(("reference", &segments)),
        )?;
        let body = catalog.get(&route, &[])?;
        let project = catalog.project(&body)?;
        if !catalog.listed_for_game(&body, &self.program.mappings.project)? {
            return Err(specific(RuntimeError::ProjectNotForGame {
                project: project.label().to_owned(),
                game: target.game.clone(),
            }));
        }
        Ok(project)
    }

    fn releases(
        &self,
        http: &dyn HttpClient,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Release>, AdapterError> {
        if self.program.runtime == RuntimeKind::ToolV1 {
            let game = self.tool_game(target)?.to_owned();
            return Ok(vec![Release {
                id: TOOL_RELEASE.to_owned(),
                project: self.tool_package(project)?,
                number: TOOL_RELEASE.to_owned(),
                channel: Channel::Release,
                published: String::new(),
                files: vec![ReleaseFile {
                    download: Download::Tool {
                        game,
                        item: project.to_owned(),
                    },
                    name: project.to_owned(),
                    size: None,
                    limit: None,
                    md5: None,
                    sha1: None,
                    sha256: None,
                    sha512: None,
                    primary: true,
                }],
                dependencies: Vec::new(),
            }]);
        }
        Ok(self
            .catalog(http, target)?
            .listing(project)?
            .into_iter()
            .filter(|listed| listed.fits)
            .map(|listed| listed.release)
            .collect())
    }

    fn release_project(
        &self,
        http: &dyn HttpClient,
        release: &str,
        target: &Target,
    ) -> Result<PackageId, AdapterError> {
        if !self.has(Capability::ReleaseProject) {
            return Err(specific(RuntimeError::UnsupportedReleaseProject));
        }
        if !is_reference(release) {
            return Err(invalid_reference(&self.program, release));
        }
        let catalog = self.catalog(http, target)?;
        let route = catalog.route(
            required_route(self.program.routes.release.as_deref())?,
            Some(("release", release)),
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
        let catalog = self.catalog(http, target)?;
        match protocol {
            UpdateProtocol::HashLookupV1 {
                algorithm,
                listed,
                latest,
                fields,
            } => catalog.check_by_hash(
                &HashLookup {
                    algorithm: *algorithm,
                    listed,
                    latest,
                    fields,
                },
                installed,
            ),
            UpdateProtocol::ReleasesV1 {} => installed
                .iter()
                .map(|provenance| catalog.check_by_releases(provenance))
                .collect(),
        }
    }
}

impl Handoff for ProgramAdapter {
    fn scheme(&self) -> &str {
        self.program.provider.handoff_scheme().unwrap_or_default()
    }

    /// Checks, in order: the link's length and scheme, that its host names a game in `[games]`,
    /// that its path has exactly the declared segments, that the project and release are
    /// references, and that each declared query parameter appears once, with an expiry in the
    /// future. Undeclared query parameters are dropped.
    fn parse(&self, uri: &str, now: u64) -> Result<HandoffTicket, AdapterError> {
        let link = self
            .program
            .handoff
            .as_ref()
            .ok_or_else(|| specific(RuntimeError::NoHandoff))?;
        if uri.len() > LINK_LIMIT {
            return Err(invalid_link("is too long"));
        }
        let rest = uri
            .split_once("://")
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(self.scheme()))
            .map(|(_, rest)| rest)
            .ok_or_else(|| invalid_link("does not use this provider's scheme"))?;
        let rest = rest.split('#').next().unwrap_or_default();
        let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (host, path) = location.split_once('/').unwrap_or((location, ""));
        let game = self
            .program
            .game_with_id(host)
            .ok_or_else(|| invalid_link("names a game this provider does not serve"))?;
        let (project, release) = link_path(&link.path, path)?;
        let (query, expires) = link_query(&link.query, query)?;
        if expires.is_some_and(|expires| expires <= now) {
            return Err(specific(RuntimeError::ExpiredLink));
        }
        Ok(HandoffTicket {
            provider: self.program.provider.id.clone(),
            game: game.to_owned(),
            catalog_game: host.to_owned(),
            project: project.to_owned(),
            release: release.to_owned(),
            query,
            expires,
        })
    }

    fn redeem(
        &self,
        http: &dyn HttpClient,
        ticket: &HandoffTicket,
    ) -> Result<Redeemed, AdapterError> {
        let program = &self.program;
        let (Some(link), Some(mapping), Some(base)) = (
            program.handoff.as_ref(),
            program.mappings.handoff.as_ref(),
            program.provider.api_base(),
        ) else {
            return Err(specific(RuntimeError::NoHandoff));
        };
        if ticket.provider != program.provider.id {
            return Err(specific(RuntimeError::ForeignTicket));
        }
        // A ticket's fields are public, so what goes into the route is checked again.
        if program.game_with_id(&ticket.catalog_game) != Some(ticket.game.as_str())
            || !is_reference(&ticket.project)
            || !is_reference(&ticket.release)
        {
            return Err(invalid_link(
                "names a project or release that is not a reference",
            ));
        }
        let route = link.redeem.replacen("{game}", &ticket.catalog_game, 1);
        let route = interpolate(&route, "project", &ticket.project)?;
        let route = interpolate(&route, "release", &ticket.release)?;
        let query: Vec<(&str, &str)> = ticket
            .query
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let body: Value = JsonEndpoint::new(http, base, JSON_LIMIT).get(&route, &query)?;
        let url = select_strings(&body, &mapping.urls)?
            .into_iter()
            .next()
            .ok_or_else(|| specific(RuntimeError::NoRedeemedUrl))?;
        // A redeemed URL is a delivery address, not a description: its last segment may be an
        // opaque id with no extension, which would leave the file unnamed and its container
        // unrecognised. The catalog's own listing of the release names it, so that is preferred,
        // and the URL is the fallback for a listing that cannot be reached.
        let listed = self.listed_file(http, ticket);
        let name = match &listed {
            Some(file) => file.name.clone(),
            None => file_name_of(&url).map_err(specific)?,
        };
        let file = ReleaseFile {
            download: Download::Direct { url },
            name,
            size: listed.as_ref().and_then(|file| file.size),
            limit: listed.as_ref().and_then(|file| file.limit),
            md5: listed.as_ref().and_then(|file| file.md5.clone()),
            sha1: listed.as_ref().and_then(|file| file.sha1.clone()),
            sha256: listed.as_ref().and_then(|file| file.sha256.clone()),
            sha512: listed.as_ref().and_then(|file| file.sha512.clone()),
            primary: true,
        };
        Ok(Redeemed {
            file,
            title: self.listed_title(http, ticket),
        })
    }
}

impl ProgramAdapter {
    /// The catalog's listing of the file `ticket` names, when it lists one.
    ///
    /// Best effort: a listing MSBE cannot reach, or one that does not name the release, leaves the
    /// redeemed link to describe its own file. Redeeming already succeeded by this point, and
    /// failing it over a metadata lookup would lose a download the user is waiting on.
    fn listed_file(&self, http: &dyn HttpClient, ticket: &HandoffTicket) -> Option<ReleaseFile> {
        let catalog = self.link_catalog(http, ticket).ok()?;
        let release = catalog
            .listing(&ticket.project)
            .ok()?
            .into_iter()
            .map(|listed| listed.release)
            .find(|release| release.id == ticket.release)?;
        let primary = release.files.iter().any(|file| file.primary);
        release
            .files
            .into_iter()
            .find(|file| file.primary || !primary)
    }

    /// The catalog's name for the project `ticket` names, when it publishes one. Best effort, for
    /// the same reason as [`Self::listed_file`].
    fn listed_title(&self, http: &dyn HttpClient, ticket: &HandoffTicket) -> Option<String> {
        let catalog = self.link_catalog(http, ticket).ok()?;
        let route = catalog
            .route(
                self.program.routes.project.as_deref()?,
                Some(("reference", &ticket.project)),
            )
            .ok()?;
        let body = catalog.get(&route, &[]).ok()?;
        catalog
            .project(&body)
            .ok()
            .map(|project| project.slug.unwrap_or(project.title))
    }
}

/// The project and release in a link's `path`, which must have exactly the `declared` segments.
fn link_path<'l>(declared: &[String], path: &'l str) -> Result<(&'l str, &'l str), AdapterError> {
    let segments: Vec<&str> = path.split('/').collect();
    let mismatch = || invalid_link("does not have this provider's path");
    if segments.len() != declared.len() {
        return Err(mismatch());
    }
    let (mut project, mut release) = (None, None);
    for (declared, segment) in declared.iter().zip(segments) {
        match declared.as_str() {
            "{project}" => project = Some(segment),
            "{release}" => release = Some(segment),
            literal if literal == segment => {}
            _ => return Err(mismatch()),
        }
    }
    match (project, release) {
        (Some(project), Some(release)) if is_reference(project) && is_reference(release) => {
            Ok((project, release))
        }
        (Some(_), Some(_)) => Err(invalid_link(
            "names a project or release that is not a reference",
        )),
        _ => Err(mismatch()),
    }
}

/// The query parameters a handoff link keeps, by name, and its expiry.
type LinkQuery = (Vec<(String, String)>, Option<u64>);

/// The declared parameters in a link's `query`, percent-decoded, and the expiry among them. Each
/// declared parameter must appear exactly once; every other is dropped.
fn link_query(declared: &HandoffQuery, query: &str) -> Result<LinkQuery, AdapterError> {
    let names = [declared.key.as_deref(), declared.expires.as_deref()];
    let malformed = || invalid_link("has a missing, repeated or malformed query parameter");
    let mut kept: Vec<(String, String)> = Vec::new();
    for pair in query.split('&') {
        let (name, raw) = pair.split_once('=').unwrap_or((pair, ""));
        if !names.contains(&Some(name)) {
            continue;
        }
        let value = percent_decode(raw)
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic()))
            .ok_or_else(malformed)?;
        if kept.iter().any(|(kept, _)| kept == name) {
            return Err(malformed());
        }
        kept.push((name.to_owned(), value));
    }
    if names
        .iter()
        .flatten()
        .any(|name| !kept.iter().any(|(kept, _)| kept == name))
    {
        return Err(malformed());
    }
    let expires = declared
        .expires
        .as_deref()
        .and_then(|name| kept.iter().find(|(kept, _)| kept == name))
        .map(|(_, value)| {
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| value.parse::<u64>().ok())
                .flatten()
                .ok_or_else(|| invalid_link("has an expiry that is not a number"))
        })
        .transpose()?;
    Ok((kept, expires))
}

fn invalid_link(reason: &'static str) -> AdapterError {
    specific(RuntimeError::InvalidLink(reason))
}

impl Accounts for ProgramAdapter {
    fn account(&self, http: &dyn HttpClient) -> Result<Account, AdapterError> {
        let program = &self.program;
        let (Some(auth), Some(mapping), Some(base)) = (
            program.auth.as_ref(),
            program.mappings.account.as_ref(),
            program.provider.api_base(),
        ) else {
            return Err(specific(RuntimeError::NoAuth));
        };
        let body: Value = JsonEndpoint::new(http, base, JSON_LIMIT).get(&auth.validate, &[])?;
        Ok(Account {
            name: required_text(&body, Some(&mapping.name))?,
            premium: mapping
                .premium
                .as_deref()
                .and_then(|pointer| body.pointer(pointer))
                .and_then(Value::as_bool),
        })
    }
}

/// The slots of a `hash-lookup-v1` protocol.
struct HashLookup<'a> {
    algorithm: HashAlgorithm,
    listed: &'a str,
    latest: &'a str,
    fields: &'a UpdateFields,
}

/// A listed release, and whether it supports the target.
struct Listed {
    release: Release,
    fits: bool,
}

/// One catalog program's view of its API, for one operation against one target.
struct Catalog<'a> {
    program: &'a ProviderProgram,
    endpoint: JsonEndpoint<'a>,
    /// What the catalog's answers are judged against, which a catalog opened for a handoff link
    /// does not have: such a link names a game, and nothing about the installation it is bound for.
    /// A fact the target would supply is then absent, and every check that needs one is skipped.
    target: Option<&'a Target>,
    /// The catalog's identifier for the game.
    game: &'a str,
}

impl<'a> Catalog<'a> {
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

    /// `template` with `{game}` and the one named placeholder filled in. `value` must already be
    /// safe as route segments.
    fn route(
        &self,
        template: &str,
        placeholder: Option<(&str, &str)>,
    ) -> Result<String, AdapterError> {
        let route = template.replacen("{game}", self.game, 1);
        match placeholder {
            Some((name, value)) => interpolate(&route, name, value),
            None => Ok(route),
        }
    }

    /// `raw`, a project reference or id, as route segments.
    fn segments(&self, raw: &str) -> Result<String, AdapterError> {
        route_segments(self.program, raw).ok_or_else(|| invalid_reference(self.program, raw))
    }

    fn package(&self, project: String) -> PackageId {
        PackageId {
            provider: self.program.provider.id.clone(),
            project,
        }
    }

    /// The catalog's spellings of the target's values for `fact`: `None` when the target has no
    /// such fact, and empty when the catalog has a spelling for none of them.
    fn fact(&self, fact: TargetFact) -> Option<Vec<&'a str>> {
        let values: Vec<&'a str> = match fact {
            TargetFact::Game => return Some(vec![self.game]),
            TargetFact::Loaders => self.target?.loader_ids().collect(),
            TargetFact::GameVersion => vec![self.target?.game_version.as_deref()?],
            TargetFact::Edition => vec![self.target?.edition.as_deref()?],
            TargetFact::Storefront => vec![self.target?.storefront.as_deref()?],
        };
        Some(match self.program.translate.table(fact) {
            Some(table) if !table.is_empty() => distinct(
                values
                    .into_iter()
                    .filter_map(|value| table.get(value).map(String::as_str)),
            ),
            _ => values,
        })
    }

    /// A parameter's values: its own spellings of MSBE's values when it declares them, else the
    /// program's.
    fn parameter_values(
        &self,
        parameter: &'a QueryParameter,
        fact: TargetFact,
    ) -> Option<Vec<&'a str>> {
        if parameter.values.is_empty() {
            return self.fact(fact);
        }
        let target = self.target?;
        let values: Vec<&'a str> = match fact {
            TargetFact::Game => vec![target.game.as_str()],
            TargetFact::Loaders => target.loader_ids().collect(),
            TargetFact::GameVersion => vec![target.game_version.as_deref()?],
            TargetFact::Edition => vec![target.edition.as_deref()?],
            TargetFact::Storefront => vec![target.storefront.as_deref()?],
        };
        Some(distinct(values.into_iter().filter_map(|value| {
            parameter.values.get(value).map(String::as_str)
        })))
    }

    /// `parameters` for the target, encoded. A fact the target lacks, or has no spelling for, is
    /// not sent.
    fn parameters(&self, parameters: &'a [QueryParameter]) -> Vec<(String, String)> {
        let mut encoded = Vec::new();
        for parameter in parameters {
            let name = parameter.name.clone();
            if let Some(literal) = &parameter.literal {
                encoded.push((name, literal.clone()));
                continue;
            }
            let Some(values) = parameter
                .target
                .and_then(|fact| self.parameter_values(parameter, fact))
                .filter(|values| !values.is_empty())
            else {
                continue;
            };
            match (parameter.encoding, values.as_slice()) {
                (Encoding::JsonArray, _) => {
                    encoded.push((name, serde_json::to_string(&values).unwrap_or_default()));
                }
                (Encoding::Comma, _) => encoded.push((name, values.join(","))),
                (Encoding::Repeated, _) => encoded.extend(
                    values
                        .iter()
                        .map(|value| (name.clone(), (*value).to_owned())),
                ),
                (Encoding::Single, [value]) => encoded.push((name, (*value).to_owned())),
                (Encoding::Single, _) => {}
            }
        }
        encoded
    }

    /// The facet groups for the target, as a JSON array of arrays of strings.
    fn facet_groups(&self, facets: &Facets) -> String {
        let groups: Vec<Vec<String>> = facets
            .groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .flat_map(|template| self.expand(template))
                    .collect::<Vec<_>>()
            })
            .filter(|group| !group.is_empty())
            .collect();
        serde_json::to_string(&groups).unwrap_or_default()
    }

    /// A facet template once per value of the fact it names, or as it is when it names none.
    fn expand(&self, template: &str) -> Vec<String> {
        for (marker, fact) in [
            ("{game}", TargetFact::Game),
            ("{game_version}", TargetFact::GameVersion),
            ("{loader}", TargetFact::Loaders),
            ("{edition}", TargetFact::Edition),
            ("{storefront}", TargetFact::Storefront),
        ] {
            if template.contains(marker) {
                return self
                    .fact(fact)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|value| template.replace(marker, value))
                    .collect();
            }
        }
        vec![template.to_owned()]
    }

    /// Whether the object in `value` is listed for the target's game, when the mapping says where
    /// its games are.
    fn listed_for_game(&self, value: &Value, map: &ObjectMapping) -> Result<bool, AdapterError> {
        let Some(selector) = &map.games else {
            return Ok(true);
        };
        Ok(select_strings(value, selector)?
            .iter()
            .any(|game| game == self.game))
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

    /// Every release of `project` the catalog lists, in the program's order, each marked with
    /// whether it supports the target.
    fn listing(&self, project: &str) -> Result<Vec<Listed>, AdapterError> {
        let segments = self.segments(project)?;
        let route = self.route(
            required_route(self.program.routes.releases.as_deref())?,
            Some(("project", &segments)),
        )?;
        let parameters = self.parameters(&self.program.releases.query);
        let parameters: Vec<(&str, &str)> = parameters
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let body = self.get(&route, &parameters)?;
        let values = match &self.program.mappings.releases {
            Some(items) => select_items(&body, items)?,
            None => body
                .as_array()
                .ok_or_else(|| specific(RuntimeError::ExpectedArray))?
                .iter()
                .collect(),
        };
        let mut listed = values
            .into_iter()
            .map(|value| {
                Ok(Listed {
                    fits: self.supports(value)?,
                    release: self.release(value, Some(project))?,
                })
            })
            .collect::<Result<Vec<_>, AdapterError>>()?;
        match self.program.releases.order {
            ReleaseOrder::Listed => {}
            ReleaseOrder::NewestFirst => {
                listed.sort_by(|a, b| b.release.published.cmp(&a.release.published));
            }
            ReleaseOrder::Semver => {
                listed.sort_by(|a, b| semver_key(&b.release).cmp(&semver_key(&a.release)));
            }
        }
        Ok(listed)
    }

    /// Whether `candidate` is newer than `installed`, by the program's release order.
    fn newer(&self, candidate: &Release, installed: &Release) -> bool {
        if self.program.releases.order == ReleaseOrder::Semver
            && let (Some(candidate), Some(installed)) = (
                parse_version(&candidate.number),
                parse_version(&installed.number),
            )
        {
            return candidate > installed;
        }
        candidate.published > installed.published
    }

    /// Whether the release in `value` supports the target's game version, loaders, loader version,
    /// edition and storefront, in the catalog's spellings. A compatibility list that is unmapped
    /// does not constrain a release; see [`msbe_provider_api::ReleaseMapping`].
    fn supports(&self, value: &Value) -> Result<bool, AdapterError> {
        let map = &self.program.mappings.release;
        let lists = |selector: Option<&Selector>, fact, empty_supports| {
            let (Some(selector), Some(wanted)) = (selector, self.fact(fact)) else {
                return Ok::<bool, AdapterError>(true);
            };
            let declared = select_strings(value, selector)?;
            Ok(if declared.is_empty() {
                empty_supports
            } else {
                declared
                    .iter()
                    .any(|declared| wanted.contains(&declared.as_str()))
            })
        };
        let loader_ids = self.fact(TargetFact::Loaders).unwrap_or_default();
        let declared = map
            .loader_versions
            .as_deref()
            .and_then(|pointer| value.pointer(pointer))
            .and_then(Value::as_object);
        let wanted_version = self
            .target
            .and_then(|target| target.loader_version.as_deref());
        let version_matches = match (wanted_version, declared) {
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
        Ok(
            lists(map.game_versions.as_ref(), TargetFact::GameVersion, true)?
                && lists(map.loaders.as_ref(), TargetFact::Loaders, false)?
                && lists(map.editions.as_ref(), TargetFact::Edition, true)?
                && lists(map.storefronts.as_ref(), TargetFact::Storefront, true)?
                && version_matches,
        )
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
        let id = required_text(value, map.id.as_deref())?;
        let files = select_items(
            value,
            map.files
                .as_ref()
                .ok_or_else(|| specific(RuntimeError::MissingMapping))?,
        )?
        .into_iter()
        .map(|file| self.file(file, &project, &id))
        .collect::<Result<_, _>>()?;
        let declared = match map.dependencies.as_deref() {
            None => &[][..],
            Some(pointer) => required_items(value, Some(pointer))
                .or_else(|error| absent_as_empty(value, Some(pointer), error))?,
        };
        let dependencies = declared
            .iter()
            .map(|dependency| self.dependency(dependency, &map.dependency))
            .collect::<Result<_, _>>()?;
        Ok(Release {
            id,
            project: self.package(project),
            number: required_text(value, map.number.as_deref())?,
            channel: channel(value, map.channel.as_ref()),
            published: required_text(value, map.published.as_deref())?,
            files,
            dependencies,
        })
    }

    fn file(
        &self,
        value: &Value,
        project: &str,
        release: &str,
    ) -> Result<ReleaseFile, AdapterError> {
        let mapping = &self.program.mappings.release.file;
        let mut name = required_text(value, mapping.name.as_deref())?;
        if let Some(extension) = &mapping.extension {
            let suffix = format!(".{extension}");
            if !name
                .to_ascii_lowercase()
                .ends_with(&suffix.to_ascii_lowercase())
            {
                name.push_str(&suffix);
            }
        }
        Ok(ReleaseFile {
            download: self.download(value, project, release)?,
            name,
            size: mapping
                .size
                .as_deref()
                .and_then(|pointer| value.pointer(pointer))
                .and_then(Value::as_u64),
            // Whole KiB may be rounded either way, so one more bounds the file.
            limit: mapping
                .size_kib
                .as_deref()
                .and_then(|pointer| value.pointer(pointer))
                .and_then(Value::as_u64)
                .map(|kib| kib.saturating_add(1).saturating_mul(1024)),
            md5: select_text(value, mapping.md5.as_ref()),
            sha1: select_text(value, mapping.sha1.as_ref()),
            sha256: select_text(value, mapping.sha256.as_ref()),
            sha512: select_text(value, mapping.sha512.as_ref()),
            primary: mapping.primary.as_deref().is_none_or(|pointer| {
                value
                    .pointer(pointer)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            }),
        })
    }

    /// How the file in `value` can be obtained, under the provider's acquisition primitive and the
    /// file's own distribution flag and download URL.
    fn download(
        &self,
        value: &Value,
        project: &str,
        release: &str,
    ) -> Result<Download, AdapterError> {
        let mapping = &self.program.mappings.release.file;
        let user_action = |reason| {
            Ok(Download::UserAction {
                page: self.page(project, release)?,
                reason,
            })
        };
        match &self.program.provider.acquisition {
            Acquisition::ExternalTool {} => Err(specific(RuntimeError::NoTool)),
            Acquisition::UserAction {} => user_action(ActionReason::WebsiteOnly),
            Acquisition::BrowserAssisted { scheme } => Ok(Download::BrowserAssisted {
                page: self.page(project, release)?,
                scheme: scheme.clone(),
            }),
            Acquisition::DirectHttps {} => {
                let forbidden = mapping
                    .distributable
                    .as_deref()
                    .and_then(|pointer| value.pointer(pointer))
                    .is_some_and(|flag| *flag != Value::Bool(true));
                if forbidden {
                    return user_action(ActionReason::DistributionForbidden);
                }
                match mapping.url.as_deref().map(|pointer| value.pointer(pointer)) {
                    Some(Some(Value::String(url))) => Ok(Download::Direct { url: url.clone() }),
                    Some(None | Some(Value::Null)) if !self.program.pages.is_empty() => {
                        user_action(ActionReason::NoDownloadUrl)
                    }
                    _ => Err(required_text(value, mapping.url.as_deref())
                        .err()
                        .unwrap_or_else(|| specific(RuntimeError::ExpectedText))),
                }
            }
        }
    }

    /// The page for one release of `project`, preferring the release page.
    fn page(&self, project: &str, release: &str) -> Result<String, AdapterError> {
        let pages = &self.program.pages;
        let (template, release) = match (&pages.release, &pages.project) {
            (Some(template), _) => (template, Some(release)),
            (None, Some(template)) => (template, None),
            (None, None) => return Err(specific(RuntimeError::MissingPage)),
        };
        let mut page = template.replacen("{game}", self.game, 1);
        if page.contains("{project}") {
            page = page.replacen("{project}", &self.segments(project)?, 1);
        }
        if let Some(release) = release {
            if !is_reference(release) {
                return Err(invalid_reference(self.program, release));
            }
            page = page.replacen("{release}", release, 1);
        }
        Ok(page)
    }

    fn dependency(
        &self,
        value: &Value,
        mapping: &DependencyMapping,
    ) -> Result<Dependency, AdapterError> {
        if let Some(text) = &mapping.text {
            let raw = value
                .as_str()
                .ok_or_else(|| specific(RuntimeError::ExpectedText))?;
            let project = raw
                .rsplit_once(text.separator.as_str())
                .map(|(project, _)| project)
                .filter(|project| route_segments(self.program, project).is_some())
                .ok_or_else(|| specific(RuntimeError::InvalidDependency(raw.to_owned())))?;
            return Ok(Dependency {
                project: Some(self.package(project.to_owned())),
                release: None,
                kind: DependencyKind::Required,
            });
        }
        let named = optional_text(value, mapping.kind.as_deref());
        let named = named.as_deref();
        let kind = match &mapping.kinds {
            None => match named {
                Some("required") => DependencyKind::Required,
                Some("optional") => DependencyKind::Optional,
                Some("incompatible") => DependencyKind::Incompatible,
                Some("embedded") => DependencyKind::Embedded,
                _ => DependencyKind::Unknown,
            },
            Some(kinds) => [
                (&kinds.required, DependencyKind::Required),
                (&kinds.optional, DependencyKind::Optional),
                (&kinds.incompatible, DependencyKind::Incompatible),
                (&kinds.embedded, DependencyKind::Embedded),
            ]
            .into_iter()
            .find(|(name, _)| name.is_some() && name.as_deref() == named)
            .map_or(DependencyKind::Unknown, |(_, kind)| kind),
        };
        Ok(Dependency {
            project: optional_text(value, mapping.project.as_deref())
                .map(|project| self.package(project)),
            release: optional_text(value, mapping.release.as_deref()),
            kind,
        })
    }

    /// Checks installed files for releases to replace them with, answering in the order
    /// `installed` is given, under the `hash-lookup-v1` protocol.
    ///
    /// A file stays on its release channel or moves to a more stable one. A replacement is always
    /// newer than the installed release, unless the installed release does not support the target;
    /// then the newest compatible release on its channel is offered, even if it is older. Costs
    /// one request for the installed releases and one per channel in use.
    fn check_by_hash(
        &self,
        protocol: &HashLookup<'_>,
        installed: &[&Provenance],
    ) -> Result<Vec<UpdateCheck>, AdapterError> {
        let algorithm = protocol.algorithm.as_str();
        let fields = protocol.fields;
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
            &self.route(protocol.listed, None)?,
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
        let latest_route = self.route(protocol.latest, None)?;
        let loaders = self.fact(TargetFact::Loaders).unwrap_or_default();
        let game_versions = self
            .fact(TargetFact::GameVersion)
            .filter(|versions| !versions.is_empty());
        let mut latest = BTreeMap::new();
        for (channels, hashes) in groups {
            let mut body = Map::from_iter([
                (fields.hashes.clone(), Value::from(hashes)),
                (fields.algorithm.clone(), Value::from(algorithm)),
                (fields.loaders.clone(), Value::from(loaders.clone())),
                (fields.channels.clone(), Value::from(channels)),
            ]);
            if let Some(versions) = &game_versions {
                body.insert(fields.game_versions.clone(), Value::from(versions.clone()));
            }
            latest.extend(self.post(&latest_route, body)?);
        }

        requested
            .iter()
            .map(|hash| match listed.get(hash) {
                Some(release) => self.decide(release, latest.get(hash)),
                None => Ok(UpdateCheck::Unlisted),
            })
            .collect()
    }

    /// Whether `latest`, the newest release on the installed release's channel, should replace it.
    fn decide(
        &self,
        installed: &Value,
        latest: Option<&Value>,
    ) -> Result<UpdateCheck, AdapterError> {
        let map = &self.program.mappings.release;
        let fits = self.supports(installed)?;
        let mut replacement = None;
        if let Some(candidate) = latest
            && required_text(candidate, map.id.as_deref())?
                != required_text(installed, map.id.as_deref())?
            && self.supports(candidate)?
            && (!fits
                || required_text(candidate, map.published.as_deref())?
                    > required_text(installed, map.published.as_deref())?)
        {
            replacement = Some(candidate);
        }
        Ok(match replacement {
            Some(candidate) => available(self.release(candidate, None)?)?,
            None if fits => UpdateCheck::Current,
            None => UpdateCheck::Incompatible,
        })
    }

    /// Checks one installed file under the `releases-v1` protocol, by listing its project's
    /// releases.
    ///
    /// The rules are those of `hash-lookup-v1`: a file stays on its channel or moves to a more
    /// stable one, and moves to a release that is not newer only to regain compatibility. A catalog
    /// that no longer lists the installed release is compared by version number, when the program
    /// orders releases by semantic version; otherwise the file is unlisted.
    fn check_by_releases(&self, installed: &Provenance) -> Result<UpdateCheck, AdapterError> {
        let listed = self.listing(&installed.project)?;
        let current = listed
            .iter()
            .find(|listed| listed.release.id == installed.version);
        let Some(current) = current else {
            let Some(installed_version) = parse_version(&installed.version_number)
                .filter(|_| self.program.releases.order == ReleaseOrder::Semver)
            else {
                return Ok(UpdateCheck::Unlisted);
            };
            let newer = listed.iter().find(|listed| {
                listed.fits
                    && parse_version(&listed.release.number)
                        .is_some_and(|version| version > installed_version)
            });
            return match newer {
                Some(listed) => available(listed.release.clone()),
                None if listed.is_empty() => Ok(UpdateCheck::Unlisted),
                None => Ok(UpdateCheck::Current),
            };
        };
        let mut candidates = listed.iter().filter(|candidate| {
            candidate.fits
                && candidate.release.id != current.release.id
                && at_least_as_stable(candidate.release.channel, current.release.channel)
        });
        let replacement = if current.fits {
            candidates.find(|candidate| self.newer(&candidate.release, &current.release))
        } else {
            candidates.next()
        };
        match replacement {
            Some(candidate) => available(candidate.release.clone()),
            None if current.fits => Ok(UpdateCheck::Current),
            None => Ok(UpdateCheck::Incompatible),
        }
    }
}

/// An update to `release`'s primary file.
fn available(release: Release) -> Result<UpdateCheck, AdapterError> {
    let file = release
        .primary_file()
        .cloned()
        .ok_or_else(|| AdapterError::NoFiles {
            project: release.project.clone(),
            release: release.id.clone(),
        })?;
    Ok(UpdateCheck::Available(Box::new(Update { release, file })))
}

/// `values` without repeats, in their first order.
fn distinct<'v>(values: impl Iterator<Item = &'v str>) -> Vec<&'v str> {
    let mut seen = BTreeSet::new();
    values.filter(|value| seen.insert(*value)).collect()
}

/// A release's number as a semantic version, ignoring a leading `v`.
fn parse_version(number: &str) -> Option<Version> {
    Version::parse(number.strip_prefix(['v', 'V']).unwrap_or(number)).ok()
}

/// Orders releases by semantic version, then publication; releases without one sort lowest.
fn semver_key(release: &Release) -> (Option<Version>, &str) {
    (parse_version(&release.number), release.published.as_str())
}

/// Whether a release on `candidate` may replace one on `installed`: the same channel or a more
/// stable one. A release on an unknown channel replaces only another on an unknown channel.
const fn at_least_as_stable(candidate: Channel, installed: Channel) -> bool {
    const fn rank(channel: Channel) -> u8 {
        match channel {
            Channel::Release => 0,
            Channel::Beta => 1,
            Channel::Alpha => 2,
            Channel::Unknown => 3,
        }
    }
    rank(candidate) <= rank(installed)
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
    match value.pointer(&names.pointer).and_then(scalar_text) {
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

/// A reference or project id as route segments: itself when it is one safe segment, or its parts
/// joined by `/` when the program declares segmented references.
fn route_segments(program: &ProviderProgram, raw: &str) -> Option<String> {
    match &program.routes.reference {
        None => is_reference(raw).then(|| raw.to_owned()),
        Some(shape) => {
            let parts: Vec<&str> = raw.split(shape.separator.as_str()).collect();
            (raw.len() <= REFERENCE_LIMIT
                && parts.len() == usize::from(shape.segments)
                && parts.iter().all(|part| is_reference(part)))
            .then(|| parts.join("/"))
        }
    }
}

fn invalid_reference(program: &ProviderProgram, raw: &str) -> AdapterError {
    specific(RuntimeError::InvalidReference {
        provider: program.provider.name.clone(),
        reference: raw.to_owned(),
    })
}

fn required_route(route: Option<&str>) -> Result<&str, AdapterError> {
    route.ok_or_else(|| specific(RuntimeError::MissingRoute))
}

/// Text, or an integer's decimal digits, which is how catalogs that number their identifiers are
/// read.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(number.to_string()),
        _ => None,
    }
}

fn required_text(value: &Value, pointer: Option<&str>) -> Result<String, AdapterError> {
    let pointer = pointer.ok_or_else(|| specific(RuntimeError::MissingMapping))?;
    value
        .pointer(pointer)
        .ok_or_else(|| specific(RuntimeError::MissingPointer(pointer.to_owned())))
        .and_then(|found| scalar_text(found).ok_or_else(|| specific(RuntimeError::ExpectedText)))
}

/// Text at `pointer`, or `None` when it is unmapped, absent, or not text.
fn optional_text(value: &Value, pointer: Option<&str>) -> Option<String> {
    pointer
        .and_then(|pointer| value.pointer(pointer))
        .and_then(scalar_text)
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

/// The items `items` selects; none when they are absent or null.
fn select_items<'v>(value: &'v Value, items: &Items) -> Result<Vec<&'v Value>, AdapterError> {
    match items {
        Items::Array(pointer) => Ok(required_items(value, Some(pointer))
            .or_else(|error| absent_as_empty(value, Some(pointer), error))?
            .iter()
            .collect()),
        Items::Single(single) => match value.pointer(&single.single) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(object @ Value::Object(_)) => Ok(vec![object]),
            Some(_) => Err(specific(RuntimeError::ExpectedObject)),
        },
        Items::Filtered(filtered) => Ok(required_items(value, Some(&filtered.each))
            .or_else(|error| absent_as_empty(value, Some(&filtered.each), error))?
            .iter()
            .filter(|item| kept(item, filtered))
            .collect()),
    }
}

/// Whether `item` meets `filtered`'s `when`, when declared, and not its `unless`, when declared.
fn kept(item: &Value, filtered: &FilteredItems) -> bool {
    filtered
        .when
        .as_ref()
        .is_none_or(|condition| meets(item, condition))
        && !filtered
            .unless
            .as_ref()
            .is_some_and(|condition| meets(item, condition))
}

/// Whether the value in `item` at the condition's pointer, read as text, meets `condition`.
fn meets(item: &Value, condition: &Condition) -> bool {
    condition.holds(
        item.pointer(&condition.pointer)
            .and_then(scalar_text)
            .as_deref(),
    )
}

/// The text values `selector` selects; none when they are absent or null.
fn select_strings(value: &Value, selector: &Selector) -> Result<Vec<String>, AdapterError> {
    match selector {
        Selector::Pointer(pointer) => match value.pointer(pointer) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| scalar_text(item).ok_or_else(|| specific(RuntimeError::ExpectedText)))
                .collect(),
            Some(scalar) => scalar_text(scalar)
                .map(|text| vec![text])
                .ok_or_else(|| specific(RuntimeError::ExpectedText)),
        },
        Selector::Each(each) => selected(value, each),
    }
}

/// The values inside each object `each` selects that meet its condition.
fn selected(value: &Value, each: &EachSelector) -> Result<Vec<String>, AdapterError> {
    let items = required_items(value, Some(&each.each))
        .or_else(|error| absent_as_empty(value, Some(&each.each), error))?;
    Ok(items
        .iter()
        .filter(|item| {
            each.when
                .as_ref()
                .is_none_or(|condition| meets(item, condition))
        })
        .filter_map(|item| item.pointer(&each.value).and_then(scalar_text))
        .collect())
}

/// The first text value `selector` selects, or `None`.
fn select_text(value: &Value, selector: Option<&Selector>) -> Option<String> {
    match selector? {
        Selector::Pointer(pointer) => optional_text(value, Some(pointer)),
        Selector::Each(each) => selected(value, each).ok()?.into_iter().next(),
    }
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

fn specific(error: RuntimeError) -> AdapterError {
    AdapterError::specific(error)
}

fn direct_selection(provider: &str, raw: &str) -> Result<Selection, RuntimeError> {
    let (url, checksum) = raw
        .split_once('#')
        .map_or((raw, None), |(url, fragment)| (url, Some(fragment)));
    let name = file_name_of(url)?;
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
        download: Download::Direct {
            url: url.to_owned(),
        },
        name: name.clone(),
        size: None,
        limit: None,
        md5: None,
        sha1: None,
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

/// The file name an HTTPS URL's last path segment gives, percent-decoded, when it is a safe one.
fn file_name_of(url: &str) -> Result<String, RuntimeError> {
    url.strip_prefix("https://")
        .ok_or(RuntimeError::InsecureUrl)?
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .split_once('/')
        .and_then(|(host, path)| (!host.is_empty()).then_some(path))
        .and_then(|path| path.rsplit('/').next())
        .and_then(percent_decode)
        .filter(|name| safe_file_name(name))
        .ok_or(RuntimeError::InvalidUrl)
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

/// A pinned checksum fragment. Only strong digests may pin a URL: a pin is the user's guarantee
/// that the bytes are the ones they chose.
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
    MissingPage,
    MissingPointer(String),
    ExpectedArray,
    ExpectedObject,
    ExpectedText,
    InvalidReference {
        provider: String,
        reference: String,
    },
    InvalidDependency(String),
    InvalidRouteTemplate,
    UnsupportedGame {
        provider: String,
        game: String,
        edition: Option<String>,
    },
    ProjectNotForGame {
        project: String,
        game: String,
    },
    UnsupportedReleaseProject,
    NoUpdateProtocol,
    InsecureUrl,
    InvalidUrl,
    InvalidChecksum,
    NoHandoff,
    NoAuth,
    /// Why a handoff link was refused. The link itself is never repeated: it carries a key.
    InvalidLink(&'static str),
    ExpiredLink,
    ForeignTicket,
    NoRedeemedUrl,
    NoTool,
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
            Self::InvalidDependency(dependency) => {
                return write!(
                    formatter,
                    "provider program cannot read dependency {dependency:?}"
                );
            }
            Self::UnsupportedGame {
                provider,
                game,
                edition: Some(edition),
            } => {
                return write!(
                    formatter,
                    "{provider} does not serve the {edition} edition of {game}"
                );
            }
            Self::UnsupportedGame {
                provider,
                game,
                edition: None,
            } => {
                return write!(formatter, "{provider} does not serve {game}");
            }
            Self::ProjectNotForGame { project, game } => {
                return write!(formatter, "{project} is not listed for {game}");
            }
            Self::InvalidLink(reason) => {
                return write!(formatter, "the handoff link {reason}");
            }
            Self::NoHandoff => "provider program declares no handoff links",
            Self::NoAuth => "provider program declares no way to check a credential",
            Self::ExpiredLink => {
                "the handoff link has expired; start the download again from its page"
            }
            Self::ForeignTicket => "the handoff link belongs to another provider",
            Self::NoRedeemedUrl => "the provider redeemed the handoff link without a download URL",
            Self::NoTool => "provider program has no [tool] to fetch this file with",
            Self::MissingMetadata => "provider program is missing required metadata",
            Self::MissingRoute => "provider program has no route for this operation",
            Self::MissingMapping => "provider program has no mapping for a required record field",
            Self::MissingPage => "provider program has no page to send the user to for this file",
            Self::ExpectedArray => "provider program expected a JSON array in the response",
            Self::ExpectedObject => "provider program expected a JSON object in the response",
            Self::ExpectedText => "provider program expected a JSON string in the response",
            Self::InvalidRouteTemplate => "provider program route template is invalid",
            Self::UnsupportedReleaseProject => {
                "provider program cannot resolve a release for this project"
            }
            Self::NoUpdateProtocol => "provider program declares no update protocol",
            Self::InsecureUrl => "provider program refused an insecure URL; only https is allowed",
            Self::InvalidUrl => "provider program produced an invalid URL",
            Self::InvalidChecksum => "provider program found an invalid checksum",
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

    #[test]
    fn direct_urls_are_pinned_only_by_strong_digests() {
        for weak in [
            "sha1=".to_owned() + &"a".repeat(40),
            "md5=".to_owned() + &"a".repeat(32),
        ] {
            assert!(
                direct_selection("url", &format!("https://example.test/a.jar#{weak}")).is_err()
            );
        }
    }
}
