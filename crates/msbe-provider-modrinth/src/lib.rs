//! The Modrinth provider adapter.
//!
//! Modrinth serves project metadata without authentication, publishes a dependency graph, and
//! hashes every file, which is why it is the reference adapter (`docs/06-providers-and-policy.md`).
//! Its API requires a uniquely identifying `User-Agent`, which `msbe-http` sets.
//!
//! Everything Modrinth-specific lives in this crate: its manifest and the overlay entries MSBE
//! ships about its projects, its JSON records and their translation into
//! `msbe_provider_api::model`, and the release-channel update policy with the bulk hash lookups
//! that implement it.

#[cfg(feature = "test-support")]
pub mod cli_test_support;
mod client;
mod reference;
#[cfg(test)]
mod resolution_tests;
#[cfg(test)]
mod test_support;
mod updates;
mod wire;

use msbe_provider_api::{
    Adapter, AdapterError, HttpClient, ManifestError, PackageId, Provenance, Provider,
    Registration, Releases, Search, Target, UpdateCheck, Updates,
    model::{Project, Release, Request, SearchResult},
};
use thiserror::Error;

use crate::{client::Client, reference::Spec};

/// The provider id Modrinth's manifest declares.
pub const ID: &str = "modrinth";

/// Modrinth's production API.
pub const API_BASE: &str = "https://api.modrinth.com/v2";

/// How Modrinth joins MSBE.
pub const REGISTRATION: Registration = Registration {
    id: ID,
    manifest: include_str!("../manifest.toml"),
    overlay: &[
        include_str!("../overlays/Aqlf1Shp.toml"),
        include_str!("../overlays/qvIfYCYJ.toml"),
    ],
    build,
};

fn build(provider: &Provider) -> Result<Box<dyn Adapter>, ManifestError> {
    let api_base = provider
        .api_base()
        .ok_or_else(|| ManifestError::MissingMetadata(provider.id.clone()))?;
    Ok(Box::new(Modrinth::with_base(api_base)))
}

/// The Modrinth adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modrinth {
    api_base: String,
}

impl Modrinth {
    /// An adapter for the production API.
    pub fn new() -> Self {
        Self::with_base(API_BASE)
    }

    /// An adapter for a manifest-validated deployment of the API, such as staging.
    pub fn with_base(api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
        }
    }

    fn client<'a>(&self, http: &'a dyn HttpClient) -> Client<'a> {
        Client::new(http, &self.api_base)
    }
}

impl Default for Modrinth {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Modrinth {
    fn id(&self) -> &str {
        ID
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        let spec = Spec::parse(reference)?;
        Ok(Request::Project {
            reference: spec.project,
            version: spec.version,
        })
    }

    fn as_search(&self) -> Option<&dyn Search> {
        Some(self)
    }

    fn as_releases(&self) -> Option<&dyn Releases> {
        Some(self)
    }

    fn as_updates(&self) -> Option<&dyn Updates> {
        Some(self)
    }
}

impl Search for Modrinth {
    fn search(
        &self,
        http: &dyn HttpClient,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, AdapterError> {
        Ok(self
            .client(http)
            .search(query, target, limit)?
            .into_iter()
            .filter(|hit| target.supports_side(hit.client_side, hit.server_side))
            .map(|hit| SearchResult {
                provider: ID.to_owned(),
                project: hit.project_id,
                reference: hit.slug,
                title: hit.title,
                description: hit.description,
                downloads: hit.downloads,
            })
            .collect())
    }
}

impl Releases for Modrinth {
    fn project(&self, http: &dyn HttpClient, reference: &str) -> Result<Project, AdapterError> {
        Ok(self.client(http).project(reference)?.into_model())
    }

    /// Newest first by publication date, whatever the release channel.
    fn releases(
        &self,
        http: &dyn HttpClient,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Release>, AdapterError> {
        let mut versions = self.client(http).versions(project, target)?;
        versions.retain(|version| version.supports(target));
        versions.sort_by(|a, b| b.date_published.cmp(&a.date_published));
        Ok(versions
            .into_iter()
            .map(wire::Version::into_model)
            .collect())
    }

    fn release_project(
        &self,
        http: &dyn HttpClient,
        release: &str,
    ) -> Result<PackageId, AdapterError> {
        Ok(package(&self.client(http).version(release)?.project_id))
    }
}

impl Updates for Modrinth {
    fn check(
        &self,
        http: &dyn HttpClient,
        installed: &[&Provenance],
        target: &Target,
    ) -> Result<Vec<UpdateCheck>, AdapterError> {
        updates::check(&self.client(http), installed, target)
    }
}

/// Why Modrinth refused a reference before sending it.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModrinthError {
    /// A project or version reference is malformed.
    #[error("{0:?} is not a valid Modrinth project or version reference")]
    InvalidReference(String),
}

impl From<ModrinthError> for AdapterError {
    fn from(error: ModrinthError) -> Self {
        Self::specific(error)
    }
}

/// The identity of the Modrinth project with id `project`.
fn package(project: &str) -> PackageId {
    PackageId {
        provider: ID.to_owned(),
        project: project.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use msbe_provider_api::{
        AcquisitionError, Adapter, AdapterError, Overlay, PackageId, Releases, Search,
        model::Channel,
    };
    use serde_json::{Value, json};

    use super::{ID, Modrinth, REGISTRATION};
    use crate::test_support::{BASE, catalogue, contents, target};

    #[test]
    fn releases_are_filtered_by_target_and_listed_newest_first() {
        let http = catalogue();
        let releases = Modrinth::new()
            .releases(&http, "AANobbMI", &target())
            .unwrap();
        let listed: Vec<(&str, Channel)> = releases
            .iter()
            .map(|release| (release.id.as_str(), release.channel))
            .collect();
        assert_eq!(listed, [("S2", Channel::Beta), ("S1", Channel::Release)]);
        assert!(http.requests.borrow().iter().any(|request| {
            request.contains("/project/AANobbMI/version?")
                && request.contains(r#"loaders=["fabric"]"#)
                && request.contains(r#"game_versions=["1.21.1"]"#)
                && request.contains("include_changelog=false")
        }));
    }

    #[test]
    fn target_prefilter_honours_loader_capabilities_and_loader_version() {
        let mut http = catalogue();
        let modrinth = Modrinth::new();
        let mut quilt = target();
        quilt.loader = "quilt".to_owned();
        quilt.provides = vec!["fabric".to_owned()];
        assert_eq!(
            modrinth.releases(&http, "AANobbMI", &quilt).unwrap().len(),
            2
        );

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
        assert!(
            modrinth
                .releases(&http, "AANobbMI", &mismatched)
                .unwrap()
                .is_empty()
        );
        mismatched.loader_version = Some("0.16".to_owned());
        assert_eq!(
            modrinth
                .releases(&http, "AANobbMI", &mismatched)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn downloads_are_verified_before_they_are_trusted() {
        let mut http = catalogue();
        let modrinth = Modrinth::new();
        let release = modrinth
            .releases(&http, "AANobbMI", &target())
            .unwrap()
            .into_iter()
            .find(|release| release.id == "S1")
            .unwrap();
        let file = release.primary_file().unwrap().clone();
        let good = tempfile::tempdir().unwrap();
        let acquired = modrinth.acquire(&http, &file, good.path()).unwrap();
        assert_eq!(
            std::fs::read(&acquired.path).unwrap(),
            contents("sodium-0.8.12.jar")
        );
        let provenance = modrinth.provenance(&release, &acquired);
        assert_eq!(
            (provenance.provider.as_str(), provenance.version.as_str()),
            (ID, "S1")
        );
        assert_eq!(provenance.hashes.get("sha512"), file.sha512.as_ref());

        // Same size, one bit different: only the hash can catch it.
        let mut tampered = contents("sodium-0.8.12.jar");
        if let Some(last) = tampered.last_mut() {
            *last ^= 1;
        }
        http.files.insert(file.url.clone(), tampered);
        let bad = tempfile::tempdir().unwrap();
        assert!(matches!(
            modrinth.acquire(&http, &file, bad.path()),
            Err(AdapterError::Acquisition(
                AcquisitionError::HashMismatch { .. }
            ))
        ));

        let mut insecure = file.clone();
        insecure.url = insecure.url.replacen("https://", "http://", 1);
        assert!(matches!(
            modrinth.acquire(&http, &insecure, bad.path()),
            Err(AdapterError::Acquisition(AcquisitionError::InsecureUrl(_)))
        ));

        let mut escaping = file;
        escaping.name = "../escape.jar".to_owned();
        assert!(matches!(
            modrinth.acquire(&http, &escaping, bad.path()),
            Err(AdapterError::Acquisition(AcquisitionError::UnsafeFileName(
                _
            )))
        ));
    }

    #[test]
    fn search_asks_for_mods_compatible_with_the_target() {
        let mut http = crate::test_support::FakeHttp::default();
        http.route(
            "/search",
            json!({
                "hits": [{ "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
                           "description": "A rendering engine", "downloads": 42,
                           "client_side": "required", "server_side": "required", "author": "ignored" }],
                "offset": 0, "limit": 5, "total_hits": 1
            }),
        );
        let hits = Modrinth::new()
            .search(&http, "render", &target(), 5)
            .unwrap();
        let [hit] = hits.as_slice() else {
            panic!("expected one hit, got {hits:?}");
        };
        assert_eq!(
            (hit.provider.as_str(), hit.reference.as_str()),
            (ID, "sodium")
        );

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

    #[test]
    fn ships_overlay_entries_for_fabric_api_reimplementations() {
        let overlay = Overlay::from_toml(REGISTRATION.overlay).unwrap();
        let fabric_api = PackageId {
            provider: ID.to_owned(),
            project: "P7dR8mSH".to_owned(),
        };
        let suppliers: Vec<&str> = overlay
            .suppliers(&fabric_api)
            .map(|package| package.project.as_str())
            .collect();
        assert_eq!(suppliers, ["Aqlf1Shp", "qvIfYCYJ"]);
    }

    #[test]
    fn malformed_references_are_refused_before_any_request() {
        let error = Modrinth::new().request("../etc/passwd").unwrap_err();
        assert!(
            error.to_string().contains("not a valid Modrinth"),
            "{error}"
        );
    }
}
