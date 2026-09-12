//! Portable modpack manifest import and export.
//!
//! Pack formats describe portable collections rather than provider behavior. Importing a pack
//! never downloads content. Modrinth URLs remain metadata for reviewed acquisition, while
//! CurseForge identifiers remain metadata until an official adapter is available.

pub mod host;
pub mod native;

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use msbe_fsops::RelPath;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

const INDEX: &str = "modrinth.index.json";
const LIMIT: u64 = 16 << 20;

/// A verified local file included in an exported pack as an override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFile {
    /// Portable destination relative to the game directory.
    pub path: RelPath,
    /// Local content-addressed blob to copy into the pack.
    pub source: PathBuf,
}

/// Writes a Modrinth `.mrpack` with local files as portable overrides.
///
/// # Errors
///
/// Returns [`PackError`] when an input file cannot be read or the ZIP cannot be written.
pub fn export_modrinth(
    output: &Path,
    name: Option<&str>,
    dependencies: BTreeMap<String, String>,
    files: &[ExportFile],
) -> Result<(), PackError> {
    let file = File::create(output).map_err(|source| PackError::Io { source })?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let index = ModrinthExportIndex {
        format_version: 1,
        game: "minecraft",
        version_id: "1.0.0",
        name,
        dependencies,
    };
    archive
        .start_file(INDEX, options)
        .map_err(|error| PackError::Archive(error.to_string()))?;
    archive
        .write_all(&serde_json::to_vec_pretty(&index).map_err(PackError::Json)?)
        .map_err(|source| PackError::Io { source })?;
    for export in files {
        let mut source = File::open(&export.source).map_err(|source| PackError::Io { source })?;
        archive
            .start_file(format!("overrides/{}", export.path), options)
            .map_err(|error| PackError::Archive(error.to_string()))?;
        io::copy(&mut source, &mut archive).map_err(|source| PackError::Io { source })?;
    }
    archive
        .finish()
        .map_err(|error| PackError::Archive(error.to_string()))?;
    Ok(())
}

/// A locally imported pack manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pack {
    /// A Modrinth `.mrpack` index.
    Modrinth(ModrinthPack),
    /// A CurseForge manifest.
    CurseForge(CurseForgePack),
}

/// The dependency files recorded in a Modrinth pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModrinthPack {
    /// Display name when supplied.
    pub name: Option<String>,
    /// Downloadable files.
    pub files: Vec<ModrinthFile>,
}

/// One downloadable Modrinth pack file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModrinthFile {
    /// Intended game-relative path from the Modrinth pack manifest.
    pub path: RelPath,
    /// Candidate download URLs.
    pub downloads: Vec<String>,
    /// Published hashes keyed by algorithm.
    pub hashes: BTreeMap<String, String>,
    /// Needed on client.
    pub client: bool,
    /// Needed on dedicated server.
    pub server: bool,
}

/// Metadata-only CurseForge pack references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurseForgePack {
    /// Display name when supplied.
    pub name: Option<String>,
    /// Project/file references.
    pub files: Vec<CurseForgeFile>,
}

/// One CurseForge project file named by a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurseForgeFile {
    /// CurseForge project ID.
    pub project_id: u64,
    /// CurseForge file ID.
    pub file_id: u64,
    /// Whether required.
    pub required: bool,
}

/// Imports a `.mrpack` archive or CurseForge `manifest.json` without downloading content.
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
    let mut archive = ZipArchive::new(File::open(path).map_err(|source| PackError::Io { source })?)
        .map_err(|error| PackError::Archive(error.to_string()))?;
    let mut bytes = Vec::new();
    archive
        .by_name(INDEX)
        .map_err(|_| PackError::MissingIndex)?
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
                path: file.path,
                downloads: file.downloads,
                hashes: file.hashes,
                client: file.env.client.required(),
                server: file.env.server.required(),
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
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModrinthExportIndex<'a> {
    format_version: u32,
    game: &'static str,
    version_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    dependencies: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct ModrinthIndexFile {
    path: RelPath,
    downloads: Vec<String>,
    hashes: BTreeMap<String, String>,
    #[serde(default)]
    env: Environment,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct Environment {
    #[serde(default)]
    client: EnvironmentRequirement,
    #[serde(default)]
    server: EnvironmentRequirement,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum EnvironmentRequirement {
    Boolean(bool),
    State(EnvironmentState),
}

impl Default for EnvironmentRequirement {
    fn default() -> Self {
        Self::State(EnvironmentState::Required)
    }
}

impl EnvironmentRequirement {
    const fn required(&self) -> bool {
        match self {
            Self::Boolean(required) => *required,
            Self::State(EnvironmentState::Required) => true,
            Self::State(EnvironmentState::Optional | EnvironmentState::Unsupported) => false,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EnvironmentState {
    Required,
    Optional,
    Unsupported,
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

/// Why pack import or export failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PackError {
    /// The file could not be read or written.
    #[error("cannot access pack: {source}")]
    Io {
        /// The underlying filesystem error.
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

#[cfg(test)]
mod tests {
    use super::Environment;

    #[test]
    fn environment_accepts_official_states_and_legacy_booleans() -> Result<(), serde_json::Error> {
        let environment: Environment =
            serde_json::from_str(r#"{"client":"required","server":"unsupported"}"#)?;
        assert!(environment.client.required());
        assert!(!environment.server.required());

        let environment: Environment = serde_json::from_str(r#"{"client":true,"server":false}"#)?;
        assert!(environment.client.required());
        assert!(!environment.server.required());

        let environment: Environment = serde_json::from_str(r#"{"client":"optional"}"#)?;
        assert!(!environment.client.required());
        assert!(environment.server.required());
        Ok(())
    }
}
