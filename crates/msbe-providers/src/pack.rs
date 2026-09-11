//! Metadata-only import for supported modpack manifests.
//!
//! Importing a pack never downloads its contents. Modrinth file URLs are returned for the
//! existing reviewed direct acquisition path; CurseForge file identifiers remain metadata until
//! the official CurseForge adapter is available.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::Path,
};

use serde::Deserialize;
use thiserror::Error;
use zip::ZipArchive;

const INDEX: &str = "modrinth.index.json";
const LIMIT: u64 = 16 << 20;

/// A locally imported pack manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pack {
    /// A Modrinth `.mrpack` index with verified-file metadata.
    Modrinth(ModrinthPack),
    /// A CurseForge `manifest.json` with metadata-only file references.
    CurseForge(CurseForgePack),
}

/// The dependency files recorded in a Modrinth pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModrinthPack {
    /// Pack display name, when supplied by the index.
    pub name: Option<String>,
    /// Downloadable files, with provider-published hashes.
    pub files: Vec<ModrinthFile>,
}

/// One downloadable Modrinth pack file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModrinthFile {
    /// Candidate download URLs. The first compatible reviewed source is selected by the caller.
    pub downloads: Vec<String>,
    /// Published digests keyed by algorithm, such as `sha512`.
    pub hashes: BTreeMap<String, String>,
    /// Whether the file is needed on the client.
    pub client: bool,
    /// Whether the file is needed on the dedicated server.
    pub server: bool,
}

/// Metadata-only CurseForge pack references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurseForgePack {
    /// Pack display name, when supplied by the manifest.
    pub name: Option<String>,
    /// CurseForge project/file identifiers; no download URL is inferred.
    pub files: Vec<CurseForgeFile>,
}

/// One CurseForge project file named by a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurseForgeFile {
    /// CurseForge project ID.
    pub project_id: u64,
    /// CurseForge file ID.
    pub file_id: u64,
    /// Whether the pack requires this file.
    pub required: bool,
}

/// Imports a `.mrpack` archive or a CurseForge `manifest.json` without downloading content.
///
/// # Errors
///
/// Returns [`PackError`] for unreadable, malformed, or oversized manifests.
pub fn import(path: &Path) -> Result<Pack, PackError> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mrpack"))
    {
        return import_modrinth(path).map(Pack::Modrinth);
    }
    import_curseforge(path).map(Pack::CurseForge)
}

fn import_modrinth(path: &Path) -> Result<ModrinthPack, PackError> {
    let file = File::open(path).map_err(|source| PackError::Io { source })?;
    let mut archive =
        ZipArchive::new(file).map_err(|error| PackError::Archive(error.to_string()))?;
    let entry = archive
        .by_name(INDEX)
        .map_err(|_| PackError::MissingIndex)?;
    let mut bytes = Vec::new();
    entry
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| PackError::Io { source })?;
    if u64::try_from(bytes.len()).unwrap_or_default() > LIMIT {
        return Err(PackError::TooLarge);
    }
    let index: ModrinthIndex = serde_json::from_slice(&bytes).map_err(PackError::Json)?;
    if index.format_version != 1 {
        return Err(PackError::UnsupportedVersion(index.format_version));
    }
    Ok(ModrinthPack {
        name: index.name,
        files: index
            .files
            .into_iter()
            .map(|file| ModrinthFile {
                downloads: file.downloads,
                hashes: file.hashes,
                client: file.env.client,
                server: file.env.server,
            })
            .collect(),
    })
}

fn import_curseforge(path: &Path) -> Result<CurseForgePack, PackError> {
    let bytes = std::fs::read(path).map_err(|source| PackError::Io { source })?;
    if u64::try_from(bytes.len()).unwrap_or_default() > LIMIT {
        return Err(PackError::TooLarge);
    }
    let manifest: CurseForgeManifest = serde_json::from_slice(&bytes).map_err(PackError::Json)?;
    Ok(CurseForgePack {
        name: manifest.name,
        files: manifest
            .files
            .into_iter()
            .map(|file| CurseForgeFile {
                project_id: file.project_id,
                file_id: file.file_id,
                required: file.required,
            })
            .collect(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModrinthIndex {
    format_version: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    files: Vec<ModrinthIndexFile>,
}
#[derive(Deserialize)]
struct ModrinthIndexFile {
    downloads: Vec<String>,
    hashes: BTreeMap<String, String>,
    #[serde(default)]
    env: Environment,
}
#[derive(Default, Deserialize)]
struct Environment {
    #[serde(default = "yes")]
    client: bool,
    #[serde(default = "yes")]
    server: bool,
}
const fn yes() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurseForgeManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    files: Vec<CurseForgeManifestFile>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurseForgeManifestFile {
    project_id: u64,
    file_id: u64,
    #[serde(default = "yes")]
    required: bool,
}

/// Why pack import failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PackError {
    /// The file could not be read.
    #[error("cannot read pack: {source}")]
    Io {
        /// The underlying file-read error.
        #[source]
        source: io::Error,
    },
    /// The `.mrpack` container is malformed.
    #[error("invalid .mrpack archive: {0}")]
    Archive(String),
    /// The `.mrpack` archive lacks its required index.
    #[error(".mrpack is missing {INDEX}")]
    MissingIndex,
    /// The manifest exceeds the bounded parser input.
    #[error("pack manifest exceeds the {LIMIT}-byte limit")]
    TooLarge,
    /// Manifest JSON is malformed.
    #[error("invalid pack manifest JSON: {0}")]
    Json(serde_json::Error),
    /// This Modrinth pack schema version is unsupported.
    #[error("unsupported Modrinth pack format version {0}")]
    UnsupportedVersion(u32),
}
