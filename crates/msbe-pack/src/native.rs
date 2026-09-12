//! Deterministic native `.msbepack` codec.

use std::collections::{BTreeMap, BTreeSet};

use msbe_core::instance::Lockfile;
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    ContainerKind, EmbeddedBlob, EntryContent, ImportedTarget, LayoutEntry, PackCodec,
    PackCodecDescriptor, PackCodecError, PackDirections, PackExportContext, PackExportPlan,
    PackImportContext, PackImportPlan, PackInput, PackLayout, PackOptionSchema, PackOptions,
    PackProbe, PackRequirement, SupportSet,
};
use serde::{Deserialize, Serialize};

const CODEC_ID: &str = "msbe-native";
const MANIFEST_PATH: &str = "msbe-pack.toml";
const LOCK_PATH: &str = "lock.toml";
const REQUIREMENTS_PATH: &str = "requirements.toml";
const OPTIONS_PATH: &str = "metadata/export-options.toml";
const DOCUMENT_LIMIT: u64 = 16 << 20;

/// Provider-neutral native bundle codec.
#[derive(Debug)]
pub struct NativeCodec {
    descriptor: PackCodecDescriptor,
}

impl NativeCodec {
    /// Creates the schema-1 native codec.
    #[must_use]
    pub fn new() -> Self {
        Self {
            descriptor: PackCodecDescriptor {
                id: CODEC_ID.to_owned(),
                provider: None,
                name: "MSBE native bundle".to_owned(),
                extensions: vec!["msbepack".to_owned()],
                media_types: vec!["application/vnd.msbe.pack+zip".to_owned()],
                directions: PackDirections {
                    import: true,
                    export: true,
                },
                supported_games: SupportSet::Universal,
                option_schema: PackOptionSchema {
                    schema: 1,
                    presets: Vec::new(),
                    fields: Vec::new(),
                    constraints: Vec::new(),
                },
            },
        }
    }
}

impl Default for NativeCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl PackCodec for NativeCodec {
    fn descriptor(&self) -> &PackCodecDescriptor {
        &self.descriptor
    }

    fn probe(&self, input: &dyn PackInput) -> Result<PackProbe, PackCodecError> {
        if input.container() != ContainerKind::Zip {
            return Ok(no_match());
        }
        let manifest = match read_entry(input, MANIFEST_PATH) {
            Ok(bytes) => bytes,
            Err(PackCodecError::FormatMismatch) => return Ok(no_match()),
            Err(error) => return Err(error),
        };
        let manifest: NativeManifest = match parse_toml(&manifest) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(no_match()),
        };
        Ok(PackProbe {
            confidence: u8::from(manifest.schema == 1 && manifest.format == CODEC_ID) * 100,
            reason: Some("native manifest".to_owned()),
        })
    }

    fn plan_import(
        &self,
        input: &dyn PackInput,
        _: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        self.descriptor.option_schema.normalize(options)?;
        if input.container() != ContainerKind::Zip {
            return Err(PackCodecError::FormatMismatch);
        }
        let manifest: NativeManifest = parse_toml(&read_entry(input, MANIFEST_PATH)?)?;
        if manifest.schema != 1 || manifest.format != CODEC_ID {
            return Err(PackCodecError::FormatMismatch);
        }
        let lockfile: Lockfile = parse_toml(&read_entry(input, &manifest.lockfile)?)?;
        let requirements: NativeRequirements =
            parse_toml(&read_entry(input, &manifest.requirements)?)?;
        if requirements.schema != 1 {
            return Err(PackCodecError::Codec(format!(
                "unsupported native requirements schema {}",
                requirements.schema
            )));
        }
        let mut embedded = Vec::new();
        let mut seen = BTreeSet::new();
        for (path, digest) in &lockfile.deployment {
            if seen.insert(*digest) {
                let entry = blob_path(digest)?;
                let entry_path = RelPath::new(&entry)
                    .map_err(|error| PackCodecError::Codec(format!("invalid native blob path: {error}")))?;
                if let Ok(blob) = input.read(&entry_path, DOCUMENT_LIMIT.max(digest_size_limit())) {
                    if Digest::of_bytes(&blob) != *digest {
                        return Err(PackCodecError::Codec(format!(
                            "embedded blob {entry:?} does not match {digest}"
                        )));
                    }
                    embedded.push(EmbeddedBlob {
                        entry: entry_path,
                        digest: *digest,
                        destination: Some(path.clone()),
                    });
                }
            }
        }
        Ok(PackImportPlan {
            codec: CODEC_ID.to_owned(),
            title: None,
            origin: msbe_provider_api::PackOrigin {
                codec: CODEC_ID.to_owned(),
                pack: None,
                version: None,
                digest: Digest::of_bytes(&read_entry(input, MANIFEST_PATH)?),
            },
            lockfile: Some(lockfile.clone()),
            target: ImportedTarget {
                game: Some(lockfile.plan.id.clone()),
                game_version: lockfile.target.game_version.clone(),
                loader: Some(lockfile.target.loader.clone()),
                loader_version: lockfile.target.loader_version.clone(),
            },
            environment: Vec::new(),
            requirements: requirements.requirement,
            embedded,
            warnings: Vec::new(),
        })
    }

    fn plan_export(
        &self,
        context: &PackExportContext<'_>,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError> {
        let options = self.descriptor.option_schema.normalize(options)?;
        let lock = toml::to_string(context.lockfile).map_err(codec_error)?;
        let state = serde_json::to_value(NativeState {
            lock,
            lock_schema: context.lockfile.schema,
            plan_id: context.game.id.clone(),
            plan_version: context.game.version.clone(),
        })
        .map_err(codec_error)?;
        Ok(PackExportPlan {
            codec: CODEC_ID.to_owned(),
            options,
            embedded: context.files.to_vec(),
            requirements: Vec::new(),
            warnings: Vec::new(),
            codec_state: state,
        })
    }

    fn layout(&self, plan: &PackExportPlan) -> Result<PackLayout, PackCodecError> {
        if plan.codec != CODEC_ID {
            return Err(PackCodecError::Codec(
                "export plan belongs to another codec".to_owned(),
            ));
        }
        let state: NativeState =
            serde_json::from_value(plan.codec_state.clone()).map_err(codec_error)?;
        let requirements = toml::to_string(&NativeRequirementsRef {
            schema: 1,
            requirement: &plan.requirements,
        })
        .map_err(codec_error)?;
        let options = toml::to_string(&plan.options).map_err(codec_error)?;
        let manifest = toml::to_string(&NativeManifest {
            schema: 1,
            format: CODEC_ID.to_owned(),
            created_by: env!("CARGO_PKG_VERSION").to_owned(),
            lockfile: LOCK_PATH.to_owned(),
            requirements: REQUIREMENTS_PATH.to_owned(),
            content: NativeContent {
                mode: "complete".to_owned(),
                embedded: plan.embedded.len(),
                referenced: plan.requirements.len(),
            },
            compatibility: NativeCompatibility {
                lock_schema: state.lock_schema,
                plan_id: state.plan_id,
                plan_version: state.plan_version,
            },
        })
        .map_err(codec_error)?;

        let mut entries = BTreeMap::from([
            (LOCK_PATH.to_owned(), EntryContent::Inline(state.lock.into_bytes())),
            (MANIFEST_PATH.to_owned(), EntryContent::Inline(manifest.into_bytes())),
            (OPTIONS_PATH.to_owned(), EntryContent::Inline(options.into_bytes())),
            (REQUIREMENTS_PATH.to_owned(), EntryContent::Inline(requirements.into_bytes())),
        ]);
        for file in &plan.embedded {
            let path = blob_path(&file.digest)?;
            entries.entry(path).or_insert(EntryContent::Blob(file.digest));
        }
        Ok(PackLayout {
            container: ContainerKind::Zip,
            entries: entries
                .into_iter()
                .map(|(path, content)| Ok(LayoutEntry { path: RelPath::new(&path).map_err(|error| PackCodecError::Codec(error.to_string()))?, content }))
                .collect::<Result<_, PackCodecError>>()?,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeManifest {
    schema: u32,
    format: String,
    created_by: String,
    lockfile: String,
    requirements: String,
    content: NativeContent,
    compatibility: NativeCompatibility,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeContent {
    mode: String,
    embedded: usize,
    referenced: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCompatibility {
    lock_schema: u32,
    plan_id: String,
    plan_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeState {
    lock: String,
    lock_schema: u32,
    plan_id: String,
    plan_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRequirements {
    schema: u32,
    #[serde(default)]
    requirement: Vec<PackRequirement>,
}

#[derive(Serialize)]
struct NativeRequirementsRef<'a> {
    schema: u32,
    requirement: &'a [PackRequirement],
}

fn no_match() -> PackProbe {
    PackProbe {
        confidence: 0,
        reason: None,
    }
}

fn read_entry(
    input: &dyn PackInput,
    path: &str,
) -> Result<Vec<u8>, PackCodecError> {
    input.read(&RelPath::new(path).map_err(|error| PackCodecError::Codec(error.to_string()))?, DOCUMENT_LIMIT)
}

const fn digest_size_limit() -> u64 { 4 << 30 }

fn parse_toml<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, PackCodecError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| PackCodecError::Codec(format!("native TOML is not UTF-8: {error}")))?;
    toml::from_str(text).map_err(codec_error)
}

fn blob_path(digest: &Digest) -> Result<String, PackCodecError> {
    let digest = digest.to_string();
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| PackCodecError::Codec("native bundle requires SHA-256 blobs".to_owned()))?;
    let (prefix, remainder) = hex.split_at(2);
    Ok(format!("blobs/sha256/{prefix}/{remainder}"))
}

fn codec_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use msbe_core::instance::{
        BlobSource, DistributionDecision, LockedFileClassification, LockedPlan, LockedTarget,
        Lockfile, PackFileRole,
    };
    use msbe_plan_schema::Side;
    use msbe_provider_api::{Observations, PackCodec as _, PackExportContext, PackFile};

    use super::{NativeCodec, PackCodecError};

    #[test]
    fn native_bundle_is_deterministic_and_preserves_the_lockfile() -> Result<(), PackCodecError> {
        let codec = NativeCodec::new();
        let bytes = b"pack-owned = true\n";
        let digest = msbe_fsops::Digest::of_bytes(bytes);
        let path = msbe_fsops::RelPath::new("config/example.toml")
            .map_err(|error| PackCodecError::Codec(error.to_string()))?;
        let classification = LockedFileClassification {
            role: PackFileRole::PackOwnedConfig,
            source: BlobSource::PackOwned,
        };
        let lockfile = Lockfile {
            schema: 2,
            plan: LockedPlan {
                id: "example-game".to_owned(),
                version: "1.0.0".to_owned(),
                digest: None,
            },
            target: LockedTarget {
                game_version: Some("2.0".to_owned()),
                loader: "example-loader".to_owned(),
                loader_version: Some("3.0".to_owned()),
                side: Side::Client,
                fingerprint: None,
            },
            order: Vec::new(),
            mods: BTreeMap::new(),
            components: BTreeMap::new(),
            deployment: BTreeMap::from([(path.clone(), digest)]),
            classifications: BTreeMap::from([(path.clone(), classification.clone())]),
        };
        let files = [PackFile {
            path,
            digest,
            role: classification.role,
            source: classification.source,
            distribution: DistributionDecision::Allowed,
        }];
        let context = PackExportContext {
            game: &lockfile.plan,
            target: &lockfile.target,
            lockfile: &lockfile,
            files: &files,
            observations: &Observations::default(),
        };
        let plan = codec.plan_export(&context, &BTreeMap::new())?;
        let first = codec.layout(&plan)?;
        let second = codec.layout(&plan)?;
        assert_eq!(first, second);
        let names: Vec<&str> = first.entries.iter().map(|entry| entry.path.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert!(first.entries.iter().any(|entry| matches!(
            entry.content,
            msbe_provider_api::EntryContent::Blob(found) if found == digest
        )));
        Ok(())
    }
}
