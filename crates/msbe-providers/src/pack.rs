//! Metadata-only import for supported modpack manifests.
//!
//! Importing a pack never downloads its contents. Modrinth file URLs are returned for the
//! existing reviewed direct acquisition path; CurseForge file identifiers remain metadata until
//! the official CurseForge adapter is available.

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
    /// The portable destination relative to the game directory.
    pub path: RelPath,
    /// The local content-addressed blob to copy into the pack.
    pub source: PathBuf,
}

/// Writes a Modrinth `.mrpack` with local files stored as portable overrides.
///
/// The caller supplies the selected game and loader versions as Modrinth dependency entries.
/// Artifact download URLs are deliberately not synthesized from provider metadata.
///
/// # Errors
///
/// Returns [`PackError`] when an input file cannot be read, the output cannot be created, or
/// the ZIP container cannot be written.
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
    let bytes = serde_json::to_vec_pretty(&index).map_err(PackError::Json)?;
    archive
        .write_all(&bytes)
        .map_err(|source| PackError::Io { source })?;
    for export in files {
        let entry = format!("overrides/{}", export.path);
        let mut source = File::open(&export.source).map_err(|source| PackError::Io { source })?;
        archive
            .start_file(entry, options)
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

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs::{self, File},
        io::Read,
    };

    use msbe_fsops::RelPath;

    use super::{ExportFile, export_modrinth};

    #[test]
    fn export_writes_a_portable_modrinth_override_pack() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("sodium.jar");
        let output = directory.path().join("profile.mrpack");
        fs::write(&source, b"verified bytes").unwrap();

        export_modrinth(
            &output,
            Some("Example profile"),
            BTreeMap::from([
                ("minecraft".to_owned(), "1.21.1".to_owned()),
                ("fabric-loader".to_owned(), "0.16.10".to_owned()),
            ]),
            &[ExportFile {
                path: RelPath::new("mods/sodium.jar").unwrap(),
                source,
            }],
        )
        .unwrap();

        let file = File::open(output).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut index = String::new();
        archive
            .by_name("modrinth.index.json")
            .unwrap()
            .read_to_string(&mut index)
            .unwrap();
        let index: serde_json::Value = serde_json::from_str(&index).unwrap();
        assert_eq!(index.pointer("/formatVersion"), Some(&serde_json::json!(1)));
        assert_eq!(
            index.pointer("/dependencies/minecraft"),
            Some(&serde_json::json!("1.21.1"))
        );
        assert_eq!(
            index.pointer("/dependencies/fabric-loader"),
            Some(&serde_json::json!("0.16.10"))
        );

        let mut override_file = Vec::new();
        archive
            .by_name("overrides/mods/sodium.jar")
            .unwrap()
            .read_to_end(&mut override_file)
            .unwrap();
        assert_eq!(override_file, b"verified bytes");
    }
}
