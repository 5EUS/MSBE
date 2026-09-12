//! Provider-neutral records adapters translate their native metadata into.
//!
//! Command and UI layers only ever see these. Each adapter keeps its wire formats private and
//! converts at its own boundary, so adding a provider adds no type its callers must learn.

use msbe_core::solver::PackageId;
use serde::Serialize;

use crate::Availability;

/// A content project found by a provider search.
///
/// Provider adapters translate their native result objects into this stable record before
/// returning it to command and UI layers. Provider-specific fields remain within the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    /// The stable provider identifier.
    pub provider: String,
    /// The provider's stable project identifier.
    pub project: String,
    /// The provider-specific project reference suitable for a subsequent selection.
    #[serde(rename = "slug")]
    pub reference: String,
    /// The display title.
    pub title: String,
    /// A short provider-supplied summary.
    pub description: String,
    /// A provider-supplied project icon URL, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    /// Provider-reported popularity, when available.
    pub downloads: u64,
}

/// What a user asked one provider for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// A project to resolve against the profile's target.
    Project {
        /// The provider's reference for it, such as a slug or an id.
        reference: String,
        /// A release id or version number to pin, if any.
        version: Option<String>,
    },
    /// One exact file that needs no resolution, such as a direct URL.
    File(Box<Selection>),
}

/// A project, as its provider describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    /// The provider-stable identity.
    pub id: PackageId,
    /// A readable reference that can name the mod in a profile, when the provider has one.
    pub slug: Option<String>,
    /// The display title.
    pub title: String,
    /// Whether the project runs on player clients.
    pub client: Availability,
    /// Whether the project runs on dedicated servers.
    pub server: Availability,
}

impl Project {
    /// How to refer to the project in messages: its slug, or its id when it has none.
    pub fn label(&self) -> &str {
        self.slug.as_deref().unwrap_or(&self.id.project)
    }
}

/// One published release of a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    /// The provider-stable release id.
    pub id: String,
    /// The project it belongs to.
    pub project: PackageId,
    /// The version number, as its author wrote it.
    pub number: String,
    /// How stable it is.
    pub channel: Channel,
    /// When it was published, as an RFC 3339 timestamp.
    pub published: String,
    /// Its downloadable files.
    pub files: Vec<ReleaseFile>,
    /// Its declared relationships to other projects.
    pub dependencies: Vec<Dependency>,
}

impl Release {
    /// The file to install: the one marked primary, or else the first.
    pub fn primary_file(&self) -> Option<&ReleaseFile> {
        self.files
            .iter()
            .find(|file| file.primary)
            .or_else(|| self.files.first())
    }
}

/// How stable a release is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// A stable release.
    Release,
    /// A pre-release.
    Beta,
    /// An early, unstable build.
    Alpha,
    /// A stability the provider does not declare, or that this client does not know.
    Unknown,
}

/// A downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseFile {
    /// Where to download it.
    pub url: String,
    /// The single file name to save it under.
    pub name: String,
    /// Its size in bytes, when the provider publishes it.
    pub size: Option<u64>,
    /// Its SHA-256 as hex, when the provider publishes it.
    pub sha256: Option<String>,
    /// Its SHA-512 as hex, when the provider publishes it.
    pub sha512: Option<String>,
    /// Whether it is the release's main file.
    pub primary: bool,
}

/// A release's relationship to another project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dependency {
    /// The related project, when the provider names it.
    pub project: Option<PackageId>,
    /// A specific release of it, when the provider pins one.
    pub release: Option<String>,
    /// The kind of relationship.
    pub kind: DependencyKind,
}

/// The kind of relationship a dependency declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// The project must be installed too.
    Required,
    /// The project adds functionality if installed.
    Optional,
    /// The project must not be installed alongside.
    Incompatible,
    /// The project is bundled inside this release's file.
    Embedded,
    /// A kind the provider declares that this client does not know.
    Unknown,
}

/// A release chosen for installation, and the file to install from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Selection {
    /// The project.
    pub project: Project,
    /// The chosen release.
    pub release: Release,
    /// The file to install.
    pub file: ReleaseFile,
    /// The label of the project whose requirement brought this in, if any.
    pub required_by: Option<String>,
}
