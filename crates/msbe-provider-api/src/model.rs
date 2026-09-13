//! Provider-neutral records adapters translate their native metadata into.
//!
//! Command and UI layers only ever see these. Each adapter keeps its wire formats private and
//! converts at its own boundary, so adding a provider adds no type its callers must learn.

use std::fmt;

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
    /// How the file can be obtained.
    pub download: Download,
    /// The single file name to save it under.
    pub name: String,
    /// Its size in bytes, when the provider publishes it.
    pub size: Option<u64>,
    /// The most bytes it may be, when the provider publishes only an approximate size. Never
    /// checked as an exact size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Its MD5 as hex, when the provider publishes it.
    pub md5: Option<String>,
    /// Its SHA-1 as hex, when the provider publishes it.
    pub sha1: Option<String>,
    /// Its SHA-256 as hex, when the provider publishes it.
    pub sha256: Option<String>,
    /// Its SHA-512 as hex, when the provider publishes it.
    pub sha512: Option<String>,
    /// Whether it is the release's main file.
    pub primary: bool,
}

/// How a release file can be obtained. Only [`Download::Direct`] is fetched by MSBE; the others
/// are normal outcomes that need the user, not errors to work around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Download {
    /// MSBE downloads the file from an HTTPS URL.
    Direct {
        /// Where to download it.
        url: String,
    },
    /// MSBE may not download the file: the user downloads it from a web page and adds the saved
    /// file.
    UserAction {
        /// The page the user downloads the file from.
        page: String,
        /// Why MSBE cannot download it.
        reason: ActionReason,
    },
    /// The user starts the download from a web page, whose button hands a mod manager a link with a
    /// provider-specific URI scheme.
    BrowserAssisted {
        /// The page the user starts the download from.
        page: String,
        /// The URI scheme of the link the page hands over.
        scheme: String,
    },
}

impl Download {
    /// The HTTPS URL MSBE may download from, if it may download at all.
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Direct { url } => Some(url),
            Self::UserAction { .. } | Self::BrowserAssisted { .. } => None,
        }
    }
}

impl fmt::Display for Download {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct { url } => write!(formatter, "download it from {url}"),
            Self::UserAction { page, reason } => write!(
                formatter,
                "{reason}; download it from {page}, then add the saved file"
            ),
            Self::BrowserAssisted { page, scheme } => write!(
                formatter,
                "the provider hands downloads to mod managers as {scheme}:// links, which this \
                 build cannot receive yet; download it from {page}, then add the saved file"
            ),
        }
    }
}

/// Why MSBE may not download a file itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReason {
    /// The author does not allow third-party tools to download it.
    DistributionForbidden,
    /// The provider publishes no download URL for it.
    NoDownloadUrl,
    /// The provider only serves downloads through its website.
    WebsiteOnly,
}

impl fmt::Display for ActionReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DistributionForbidden => "its author does not allow third-party downloads",
            Self::NoDownloadUrl => "its provider publishes no download URL",
            Self::WebsiteOnly => "its provider only serves downloads through its website",
        })
    }
}

/// The account a credential belongs to, as its provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    /// The account's display name.
    pub name: String,
    /// Whether the provider reports the account as paid for. Shown to the user; nothing MSBE does
    /// depends on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub premium: Option<bool>,
}

/// A handoff link read against its provider's declared structure.
///
/// Only a provider's [`Handoff`](crate::Handoff) capability builds one, after checking every part.
/// Its query values, such as a one-time key, are secret: `Debug` shows only their names, and the
/// ticket cannot be serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct HandoffTicket {
    /// The provider whose link it is.
    pub provider: String,
    /// The plan game the link's host names.
    pub game: String,
    /// The catalog's identifier for that game, as the link's host wrote it.
    pub catalog_game: String,
    /// The provider's project id.
    pub project: String,
    /// The provider's release id.
    pub release: String,
    /// The query parameters kept, under the names the link used.
    pub query: Vec<(String, String)>,
    /// When the link expires, in Unix seconds, when it says.
    pub expires: Option<u64>,
}

impl HandoffTicket {
    /// The project the linked file belongs to.
    pub fn package(&self) -> PackageId {
        PackageId {
            provider: self.provider.clone(),
            project: self.project.clone(),
        }
    }
}

impl fmt::Debug for HandoffTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.query.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("HandoffTicket")
            .field("provider", &self.provider)
            .field("game", &self.game)
            .field("project", &self.project)
            .field("release", &self.release)
            .field("query", &names)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
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
