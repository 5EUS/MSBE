//! The classified files, observations and digests pack planning works over.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
};

use msbe_core::instance::{
    BlobSource, DistributionDecision, Lockfile, PackFileRole, Profile, ProfileLayer, Provenance,
    StoredFile,
};
use msbe_fsops::{Digest, Store};
use msbe_provider_api::{Observations, PackFile};
use serde::Serialize;

use crate::{IssueCode, PackError, error::io_error};

/// The dated observation cache, kept beside the store it describes (§17.5).
const OBSERVATIONS: &str = "observations.toml";

/// The manifest that marks an instance snapshot, which no pack codec may import (§17.10).
pub(crate) const SNAPSHOT_MANIFEST: &str = "msbe-snapshot.toml";

/// Every blob `lockfile` requires, classified: each deployed path first, then artifact files the
/// plan consumes without placing, such as injection inputs, and pack-layer bases.
pub(crate) fn pack_files(profile: &Profile, lockfile: &Lockfile) -> Vec<PackFile> {
    let mut module_layers: BTreeMap<Digest, &str> = BTreeMap::new();
    for module in lockfile.mods.values() {
        for file in &module.files {
            module_layers.entry(file.blob).or_insert(&module.layer);
        }
    }
    let mut files: Vec<PackFile> = lockfile
        .deployment
        .iter()
        .map(|(path, digest)| {
            let classification = lockfile.classifications.get(path);
            let role = classification.map_or(PackFileRole::Other, |found| found.role);
            let layer = if role == PackFileRole::PackOwnedConfig {
                profile.config_layer(path, digest)
            } else {
                module_layers.get(digest).map_or_else(
                    || ProfileLayer::CHANGES.to_owned(),
                    |layer| (*layer).to_owned(),
                )
            };
            PackFile {
                path: path.clone(),
                digest: *digest,
                role,
                layer,
                deployed: true,
                source: classification.map_or(BlobSource::Unknown, |found| found.source.clone()),
                distribution: DistributionDecision::Unknown,
            }
        })
        .collect();

    let mut seen: BTreeSet<Digest> = lockfile.deployment.values().copied().collect();
    let modules = lockfile
        .mods
        .values()
        .map(|module| {
            (
                module.provider.as_ref(),
                &module.files,
                module.layer.as_str(),
            )
        })
        .chain(lockfile.layers.iter().flat_map(|layer| {
            layer
                .mods
                .values()
                .map(move |module| (module.provider.as_ref(), &module.files, layer.id.as_str()))
        }));
    for (provider, stored, layer) in modules {
        for StoredFile { source, blob } in stored {
            if seen.insert(*blob) {
                let (role, origin) = artifact_source(provider, blob);
                files.push(PackFile {
                    path: source.clone(),
                    digest: *blob,
                    role,
                    layer: layer.to_owned(),
                    deployed: false,
                    source: origin,
                    distribution: DistributionDecision::Unknown,
                });
            }
        }
    }
    for layer in &lockfile.layers {
        for (path, digest) in &layer.configs {
            if seen.insert(*digest) {
                files.push(PackFile {
                    path: path.clone(),
                    digest: *digest,
                    role: PackFileRole::PackOwnedConfig,
                    layer: layer.id.clone(),
                    deployed: false,
                    source: BlobSource::PackOwned,
                    distribution: DistributionDecision::Unknown,
                });
            }
        }
    }
    files
}

/// How an artifact file not placed by the plan is reproduced. A provider's published hash
/// identifies one file exactly only when it is that file's own digest.
fn artifact_source(provider: Option<&Provenance>, blob: &Digest) -> (PackFileRole, BlobSource) {
    match provider {
        Some(provenance) => {
            let digest = blob.to_string();
            let exact = provenance.hashes.get("sha256").map(String::as_str)
                == digest.strip_prefix("sha256:");
            (
                PackFileRole::ProviderArtifact,
                BlobSource::Provider {
                    provenance: provenance.clone(),
                    exact,
                },
            )
        }
        None => (PackFileRole::LocalArtifact, BlobSource::Local),
    }
}

/// The dated observations recorded beside `store`, or none when nothing has been observed.
pub(crate) fn observations(store: &Store) -> Result<Observations, PackError> {
    let path = store.root().join(OBSERVATIONS);
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|error| {
            PackError::issue(
                IssueCode::HostFailure,
                format!("cannot parse {}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Observations::default()),
        Err(source) => Err(io_error("read", &path)(source)),
    }
}

/// The path of the observation cache beside `store`.
pub(crate) fn observations_path(store: &Store) -> std::path::PathBuf {
    store.root().join(OBSERVATIONS)
}

/// The size of a stored blob, when the store holds it.
pub(crate) fn blob_size(store: &Store, digest: &Digest) -> Option<u64> {
    fs::metadata(store.blob_path(digest))
        .ok()
        .map(|metadata| metadata.len())
}

/// The digest of `value`'s canonical JSON encoding. Every map in a plan is ordered, so equal plans
/// have equal digests.
///
/// # Errors
///
/// Returns [`IssueCode::HostFailure`] if the value cannot be encoded.
pub fn plan_digest<T: Serialize>(value: &T) -> Result<Digest, PackError> {
    serde_json::to_vec(value)
        .map(|bytes| Digest::of_bytes(&bytes))
        .map_err(|error| PackError::issue(IssueCode::HostFailure, error.to_string()))
}
