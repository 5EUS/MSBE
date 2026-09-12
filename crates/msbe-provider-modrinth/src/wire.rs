//! Modrinth's JSON records, and their translation into the provider-neutral model.

use std::collections::BTreeMap;

use msbe_provider_api::{Availability, Target, model};
use serde::{Deserialize, Serialize};

use crate::package;

/// A Modrinth project.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Project {
    id: String,
    slug: String,
    title: String,
    #[serde(default)]
    client_side: Availability,
    #[serde(default)]
    server_side: Availability,
}

impl Project {
    /// The project as the rest of MSBE sees it.
    pub(crate) fn into_model(self) -> model::Project {
        model::Project {
            id: package(&self.id),
            slug: Some(self.slug),
            title: self.title,
            client: self.client_side,
            server: self.server_side,
        }
    }
}

/// One published version of a project.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Version {
    pub(crate) id: String,
    pub(crate) project_id: String,
    #[serde(rename = "version_number")]
    number: String,
    #[serde(rename = "version_type")]
    pub(crate) kind: VersionType,
    /// When it was published, as an RFC 3339 timestamp.
    pub(crate) date_published: String,
    #[serde(default)]
    loaders: Vec<String>,
    #[serde(default)]
    loader_versions: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    game_versions: Vec<String>,
    #[serde(default)]
    files: Vec<VersionFile>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

impl Version {
    /// Whether this version supports the target's loaders, loader version and game version.
    pub(crate) fn supports(&self, target: &Target) -> bool {
        let loader_ids: Vec<&str> = target.loader_ids().collect();
        let loader_matches = self
            .loaders
            .iter()
            .any(|loader| loader_ids.contains(&loader.as_str()));
        let version_matches = target.loader_version.as_deref().is_none_or(|wanted| {
            let declared: Vec<&str> = loader_ids
                .iter()
                .filter_map(|loader| self.loader_versions.get(*loader))
                .flatten()
                .map(String::as_str)
                .collect();
            declared.is_empty() || declared.into_iter().any(|version| version == wanted)
        });
        self.game_versions
            .iter()
            .any(|version| version == target.game_version.as_str())
            && loader_matches
            && version_matches
    }

    /// The version as the rest of MSBE sees it.
    pub(crate) fn into_model(self) -> model::Release {
        model::Release {
            id: self.id,
            project: package(&self.project_id),
            number: self.number,
            channel: self.kind.channel(),
            published: self.date_published,
            files: self
                .files
                .into_iter()
                .map(VersionFile::into_model)
                .collect(),
            dependencies: self
                .dependencies
                .into_iter()
                .map(Dependency::into_model)
                .collect(),
        }
    }
}

/// How stable a version is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VersionType {
    Release,
    Beta,
    Alpha,
    #[serde(other)]
    Unknown,
}

impl VersionType {
    const fn channel(self) -> model::Channel {
        match self {
            Self::Release => model::Channel::Release,
            Self::Beta => model::Channel::Beta,
            Self::Alpha => model::Channel::Alpha,
            Self::Unknown => model::Channel::Unknown,
        }
    }
}

/// A downloadable file of a version.
#[derive(Debug, Clone, Deserialize)]
struct VersionFile {
    hashes: Hashes,
    url: String,
    filename: String,
    #[serde(default)]
    primary: bool,
    size: u64,
}

impl VersionFile {
    fn into_model(self) -> model::ReleaseFile {
        model::ReleaseFile {
            url: self.url,
            name: self.filename,
            size: Some(self.size),
            sha256: None,
            sha512: Some(self.hashes.sha512),
            primary: self.primary,
        }
    }
}

/// The hashes Modrinth publishes for a file, of which SHA-512 is the one verified.
#[derive(Debug, Clone, Deserialize)]
struct Hashes {
    sha512: String,
}

/// A version's relationship to another project.
#[derive(Debug, Clone, Deserialize)]
struct Dependency {
    #[serde(default)]
    version_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(rename = "dependency_type")]
    kind: DependencyType,
}

impl Dependency {
    fn into_model(self) -> model::Dependency {
        model::Dependency {
            project: self.project_id.as_deref().map(package),
            release: self.version_id,
            kind: self.kind.kind(),
        }
    }
}

/// The kind of relationship a dependency declares.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DependencyType {
    Required,
    Optional,
    Incompatible,
    Embedded,
    #[serde(other)]
    Unknown,
}

impl DependencyType {
    const fn kind(self) -> model::DependencyKind {
        match self {
            Self::Required => model::DependencyKind::Required,
            Self::Optional => model::DependencyKind::Optional,
            Self::Incompatible => model::DependencyKind::Incompatible,
            Self::Embedded => model::DependencyKind::Embedded,
            Self::Unknown => model::DependencyKind::Unknown,
        }
    }
}

/// A search result.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SearchHit {
    pub(crate) project_id: String,
    pub(crate) slug: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) icon_url: Option<String>,
    #[serde(default)]
    pub(crate) downloads: u64,
    #[serde(default)]
    pub(crate) client_side: Availability,
    #[serde(default)]
    pub(crate) server_side: Availability,
}

/// A page of search results.
#[derive(Debug, Deserialize)]
pub(crate) struct SearchResults {
    pub(crate) hits: Vec<SearchHit>,
}

/// The body of Modrinth's bulk lookups by file hash.
#[derive(Debug, Serialize)]
pub(crate) struct HashQuery<'q> {
    pub(crate) hashes: &'q [&'q str],
    pub(crate) algorithm: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) loaders: Option<&'q [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) game_versions: Option<[&'q str; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version_types: Option<&'q [&'q str]>,
}
