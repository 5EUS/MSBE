//! Instance snapshots: backups, deliberately not packs (§17.10).
//!
//! A snapshot holds an instance's MSBE state and every blob it references regardless of
//! distribution terms, is marked non-distributable, and has no pack-format identity: no codec
//! imports one, and restoring one writes only MSBE state, never the game directory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use msbe_archive::Limits;
use msbe_core::{
    config::Home,
    instance::{Instance, InstanceConfig, Lockfile, Name, Profile, parse_plan},
};
use msbe_fsops::{Digest, RelPath, Store, atomic};
use msbe_provider_api::{ContainerKind, EntryContent, LayoutEntry, PackInput, PackLayout};
use serde::{Deserialize, Serialize};

use crate::{
    IssueCode, PackError, PackIssue, Progress,
    error::io_error,
    files::{self, SNAPSHOT_MANIFEST},
    host::{Compression, ZipPackInput},
    output,
    progress::checkpoint,
};

const FORMAT: &str = "msbe-snapshot";
const SCHEMA: u32 = 1;
const DOCUMENT_LIMIT: u64 = 16 << 20;
/// The most bytes a pinned extension module may be, as core enforces.
const MODULE_LIMIT: u64 = 64 << 20;
const STATE: &str = "instance/";
const BLOBS: &str = "blobs/sha256/";
const OBSERVATIONS: &str = "observations.toml";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotManifest {
    schema: u32,
    format: String,
    /// Always false: a snapshot is a backup and is never presented as shareable.
    distributable: bool,
    instance: Name,
    created_by: String,
    profiles: Vec<Name>,
    blobs: usize,
}

/// What a snapshot holds, or what a restore wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotReport {
    /// The snapshot file, or the restored instance directory.
    pub path: PathBuf,
    /// The instance.
    pub instance: Name,
    /// Its profiles.
    pub profiles: Vec<Name>,
    /// Blobs carried.
    pub blobs: usize,
    /// The snapshot file's digest.
    pub digest: Digest,
}

/// A restore, shown before anything is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestorePreview {
    /// The snapshot file.
    pub input: PathBuf,
    /// Its digest as previewed.
    pub input_digest: Digest,
    /// The instance it restores.
    pub instance: Name,
    /// The game directory the instance manages.
    pub root: PathBuf,
    /// The store its blobs are restored into.
    pub store: PathBuf,
    /// The profiles it restores.
    pub profiles: Vec<Name>,
    /// Blobs it carries.
    pub blobs: usize,
    /// Reasons the restore cannot run.
    pub blockers: Vec<PackIssue>,
}

/// Writes a snapshot of `instance` to `destination`: its configuration, pinned plan, profiles,
/// lockfiles, observation cache, and every blob they reference.
///
/// # Errors
///
/// Returns an error when the instance cannot be read, a referenced blob is missing from the store,
/// or the snapshot cannot be written.
pub fn create_snapshot(
    home: &Home,
    instance: &Name,
    destination: &Path,
    progress: &dyn Progress,
) -> Result<SnapshotReport, PackError> {
    let opened = Instance::open(home, instance)?;
    let dir = home.instance(instance);
    let mut entries: BTreeMap<String, EntryContent> = BTreeMap::new();
    for file in ["instance.toml", "plan.toml"] {
        entries.insert(
            format!("{STATE}{file}"),
            EntryContent::Inline(read(&dir.join(file))?),
        );
    }
    // The plan's pinned extension modules travel with it, so a restored instance can deploy.
    for declaration in &opened.plan().extensions {
        let module = format!("{}.wasm", declaration.sha256);
        entries.insert(
            format!("{STATE}extensions/{module}"),
            EntryContent::Inline(read(&dir.join("extensions").join(&module))?),
        );
    }
    let profiles = opened.profiles()?;
    let mut blobs = BTreeSet::new();
    for profile in &profiles {
        let path = dir.join("profiles").join(format!("{profile}.toml"));
        entries.insert(
            format!("{STATE}profiles/{profile}.toml"),
            EntryContent::Inline(read(&path)?),
        );
        blobs.extend(opened.profile(profile)?.referenced_blobs());
        let lock = dir.join("locks").join(format!("{profile}.toml"));
        if lock.is_file() {
            let bytes = read(&lock)?;
            blobs.extend(
                document::<Lockfile>(&bytes, &lock.display().to_string())?.required_blobs(),
            );
            entries.insert(
                format!("{STATE}locks/{profile}.toml"),
                EntryContent::Inline(bytes),
            );
        }
    }
    let observations = files::observations_path(opened.store());
    if observations.is_file() {
        entries.insert(
            OBSERVATIONS.to_owned(),
            EntryContent::Inline(read(&observations)?),
        );
    }
    for digest in &blobs {
        if !opened.store().contains(digest) {
            return Err(PackError::Issue(PackIssue {
                code: IssueCode::MissingBlob,
                path: None,
                digest: Some(*digest),
                message: format!("the store does not hold {digest}"),
            }));
        }
        entries.insert(blob_entry(digest)?, EntryContent::Blob(*digest));
    }
    let manifest = toml::to_string(&SnapshotManifest {
        schema: SCHEMA,
        format: FORMAT.to_owned(),
        distributable: false,
        instance: instance.clone(),
        created_by: env!("CARGO_PKG_VERSION").to_owned(),
        profiles: profiles.clone(),
        blobs: blobs.len(),
    })
    .map_err(|error| PackError::issue(IssueCode::HostFailure, error.to_string()))?;
    entries.insert(
        SNAPSHOT_MANIFEST.to_owned(),
        EntryContent::Inline(manifest.into_bytes()),
    );
    let layout = PackLayout {
        container: ContainerKind::Zip,
        entries: entries
            .into_iter()
            .map(|(path, content)| {
                Ok(LayoutEntry {
                    path: relative(&path)?,
                    content,
                })
            })
            .collect::<Result<_, PackError>>()?,
    };
    let digest = output::write(
        &layout,
        opened.store(),
        Compression::Deflate,
        destination,
        progress,
    )?;
    Ok(SnapshotReport {
        path: destination.to_path_buf(),
        instance: instance.clone(),
        profiles,
        blobs: blobs.len(),
        digest,
    })
}

/// Reads a snapshot and reports what restoring it would do.
///
/// # Errors
///
/// Returns [`IssueCode::UnknownCodec`] when the input is not an instance snapshot, or an error
/// reading it. An existing instance or missing game directory is a blocker.
pub fn preview_restore(home: &Home, input: &Path) -> Result<RestorePreview, PackError> {
    let snapshot = ZipPackInput::open(input, &Limits::default())?;
    let (manifest, config) = read_snapshot(&snapshot)?;
    let mut blockers = Vec::new();
    if home.instance(&manifest.instance).exists() {
        blockers.push(PackIssue::new(
            IssueCode::HostFailure,
            format!(
                "instance {} already exists; remove it before restoring this snapshot",
                manifest.instance
            ),
        ));
    }
    if !config.root.is_dir() {
        blockers.push(PackIssue::new(
            IssueCode::EnvironmentMismatch,
            format!(
                "the game directory {} does not exist",
                config.root.display()
            ),
        ));
    }
    Ok(RestorePreview {
        input: input.to_path_buf(),
        input_digest: snapshot.digest(),
        instance: manifest.instance,
        root: config.root,
        store: config.store,
        profiles: manifest.profiles,
        blobs: blob_entries(&snapshot).count(),
        blockers,
    })
}

/// Restores exactly the snapshot `preview` read.
///
/// Every blob is verified before the store admits it, and the instance state is staged and
/// validated beside its destination before one rename commits it. Deployment remains a separate,
/// previewed step.
///
/// # Errors
///
/// Returns [`PackError::Blocked`] for a preview with blockers, [`IssueCode::StalePlan`] when the
/// snapshot changed, [`IssueCode::IntegrityMismatch`] for a corrupt blob, or any read or write error.
pub fn restore_snapshot(
    home: &Home,
    preview: &RestorePreview,
    progress: &dyn Progress,
) -> Result<SnapshotReport, PackError> {
    if !preview.blockers.is_empty() {
        return Err(PackError::Blocked(preview.blockers.clone()));
    }
    let snapshot = ZipPackInput::open(&preview.input, &Limits::default())?;
    if snapshot.digest() != preview.input_digest {
        return Err(PackError::issue(
            IssueCode::StalePlan,
            "the snapshot changed after it was previewed",
        ));
    }
    let (manifest, config) = read_snapshot(&snapshot)?;
    let store = Store::open(&config.store)?;
    let blobs: Vec<&RelPath> = blob_entries(&snapshot).collect();
    let total = u64::try_from(blobs.len()).unwrap_or(u64::MAX);
    for (done, entry) in (0_u64..).zip(&blobs) {
        checkpoint(progress)?;
        progress.report(done, total, "verifying blobs");
        let expected = entry_digest(entry)?;
        let found = snapshot.with_entry(entry, u64::MAX, |reader| {
            store.put_reader(reader).map_err(io::Error::other)
        })?;
        if found != expected {
            return Err(PackError::Issue(PackIssue {
                code: IssueCode::IntegrityMismatch,
                path: Some((*entry).clone()),
                digest: Some(expected),
                message: format!("snapshot blob {entry} hashes to {found}"),
            }));
        }
    }
    let instances = home.instances();
    fs::create_dir_all(&instances).map_err(io_error("create", &instances))?;
    let staging = tempfile::Builder::new()
        .prefix(".msbe-restore-")
        .tempdir_in(&instances)
        .map_err(io_error("create a staging directory in", &instances))?;
    stage_state(&snapshot, &store, staging.path())?;
    if let Some(observations) = snapshot
        .entries()
        .iter()
        .find(|entry| entry.path.as_str() == OBSERVATIONS)
    {
        let cache = files::observations_path(&store);
        if !cache.exists() {
            atomic::write_file(&cache, &snapshot.read(&observations.path, DOCUMENT_LIMIT)?)?;
        }
    }
    checkpoint(progress)?;
    let destination = home.instance(&manifest.instance);
    if destination.exists() {
        return Err(PackError::issue(
            IssueCode::StalePlan,
            format!(
                "instance {} was created after the preview",
                manifest.instance
            ),
        ));
    }
    let staged = staging.keep();
    atomic::rename_replace(&staged, &destination)?;
    Instance::open(home, &manifest.instance)?;
    progress.report(total, total, "restored");
    Ok(SnapshotReport {
        path: destination,
        instance: manifest.instance,
        profiles: manifest.profiles,
        blobs: blobs.len(),
        digest: snapshot.digest(),
    })
}

/// Validates and writes every instance-state entry into `staging`.
fn stage_state(snapshot: &ZipPackInput, store: &Store, staging: &Path) -> Result<(), PackError> {
    fs::create_dir_all(staging.join("profiles")).map_err(io_error("create", staging))?;
    for entry in snapshot.entries() {
        let Some(name) = entry.path.as_str().strip_prefix(STATE) else {
            continue;
        };
        let limit = if name.starts_with("extensions/") {
            MODULE_LIMIT
        } else {
            DOCUMENT_LIMIT
        };
        let bytes = snapshot.read(&entry.path, limit)?;
        let label = entry.path.as_str();
        match name.split_once('/') {
            None if name == "instance.toml" => drop(document::<InstanceConfig>(&bytes, label)?),
            None if name == "plan.toml" => {
                drop(parse_plan(&text(&bytes, label)?, Path::new(label)).map_err(PackError::from)?);
            }
            Some(("profiles", file)) if is_toml(file) => {
                let profile = document::<Profile>(&bytes, label)?;
                if let Some(missing) = profile
                    .referenced_blobs()
                    .into_iter()
                    .find(|blob| !store.contains(blob))
                {
                    return Err(PackError::Issue(PackIssue {
                        code: IssueCode::MissingBlob,
                        path: Some(entry.path.clone()),
                        digest: Some(missing),
                        message: format!("{label} references {missing}, which the snapshot lacks"),
                    }));
                }
            }
            Some(("locks", file)) if is_toml(file) => {
                drop(document::<Lockfile>(&bytes, label)?);
            }
            Some(("extensions", file)) if is_pinned_module(file, &bytes) => {}
            _ => {
                return Err(PackError::issue(
                    IssueCode::UnsafeArchivePath,
                    format!("{label} is not instance state a snapshot may restore"),
                ));
            }
        }
        let destination = relative(name)?.to_path(staging);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(io_error("create", parent))?;
        }
        atomic::write_file(&destination, &bytes)?;
    }
    Ok(())
}

/// Whether `file` names the module `bytes` hold: `<sha256>.wasm` for their own digest.
fn is_pinned_module(file: &str, bytes: &[u8]) -> bool {
    file.strip_suffix(".wasm").is_some_and(|sha256| {
        Digest::of_bytes(bytes).to_string().strip_prefix("sha256:") == Some(sha256)
    })
}

fn read_snapshot(snapshot: &ZipPackInput) -> Result<(SnapshotManifest, InstanceConfig), PackError> {
    let not_snapshot = || {
        PackError::issue(
            IssueCode::UnknownCodec,
            "this file is not an MSBE instance snapshot",
        )
    };
    let manifest_path = relative(SNAPSHOT_MANIFEST)?;
    if !snapshot.contains(&manifest_path) {
        return Err(not_snapshot());
    }
    let manifest: SnapshotManifest = document(
        &snapshot.read(&manifest_path, DOCUMENT_LIMIT)?,
        SNAPSHOT_MANIFEST,
    )?;
    if manifest.schema != SCHEMA || manifest.format != FORMAT || manifest.distributable {
        return Err(not_snapshot());
    }
    let config_path = relative(&format!("{STATE}instance.toml"))?;
    let config: InstanceConfig = document(
        &snapshot.read(&config_path, DOCUMENT_LIMIT)?,
        config_path.as_str(),
    )?;
    if config.name != manifest.instance {
        return Err(PackError::issue(
            IssueCode::IntegrityMismatch,
            "the snapshot manifest and instance configuration name different instances",
        ));
    }
    Ok((manifest, config))
}

fn blob_entries(snapshot: &ZipPackInput) -> impl Iterator<Item = &RelPath> {
    snapshot
        .entries()
        .iter()
        .map(|entry| &entry.path)
        .filter(|path| path.as_str().starts_with(BLOBS))
}

fn blob_entry(digest: &Digest) -> Result<String, PackError> {
    let digest = digest.to_string();
    let hex = digest.strip_prefix("sha256:").ok_or_else(|| {
        PackError::issue(IssueCode::HostFailure, "snapshots require SHA-256 blobs")
    })?;
    Ok(format!(
        "{BLOBS}{}/{}",
        hex.get(..2).unwrap_or_default(),
        hex.get(2..).unwrap_or_default()
    ))
}

fn entry_digest(entry: &RelPath) -> Result<Digest, PackError> {
    let hex: String = entry
        .as_str()
        .strip_prefix(BLOBS)
        .unwrap_or_default()
        .split('/')
        .collect();
    format!("sha256:{hex}").parse().map_err(|_| {
        PackError::issue(
            IssueCode::UnsafeArchivePath,
            format!("{entry} is not a SHA-256 blob path"),
        )
    })
}

fn is_toml(file: &str) -> bool {
    !file.contains('/')
        && Path::new(file)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
}

fn relative(path: &str) -> Result<RelPath, PackError> {
    RelPath::new(path)
        .map_err(|error| PackError::issue(IssueCode::UnsafeArchivePath, error.to_string()))
}

fn read(path: &Path) -> Result<Vec<u8>, PackError> {
    fs::read(path).map_err(io_error("read", path))
}

fn text(bytes: &[u8], label: &str) -> Result<String, PackError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| PackError::issue(IssueCode::HostFailure, format!("{label} is not UTF-8")))
}

fn document<T: for<'de> Deserialize<'de>>(bytes: &[u8], label: &str) -> Result<T, PackError> {
    toml::from_str(&text(bytes, label)?).map_err(|error| {
        PackError::issue(
            IssueCode::HostFailure,
            format!("cannot parse {label}: {error}"),
        )
    })
}
