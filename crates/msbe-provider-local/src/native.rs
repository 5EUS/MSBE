//! Deterministic native `.msbepack` codec.
//!
//! The bundle is the canonical lockfile plus the blobs the host allowed it to embed. Blob bytes
//! never pass through this codec: import declares embedded entries by path and the host ingests
//! and verifies them, and export names blobs by digest for the host to stream from the store.

use std::collections::{BTreeMap, BTreeSet};

use msbe_core::instance::{InstallationFingerprint, Lockfile};
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    ContainerKind, EmbeddedBlob, EntryContent, EnvironmentRequirement, ImportedTarget, LayoutEntry,
    Observations, PackCodec, PackCodecDescriptor, PackCodecError, PackDirections,
    PackExportContext, PackExportPlan, PackFile, PackImportContext, PackImportPlan, PackInput,
    PackLayout, PackOptionSchema, PackOptionValue, PackOptions, PackOrigin, PackProbe,
    PackRequirement, SupportSet,
};
use serde::{Deserialize, Serialize};

/// The native codec's stable identifier.
pub const CODEC_ID: &str = "msbe-native";

const SCHEMA: u32 = 1;
const MANIFEST_PATH: &str = "msbe-pack.toml";
const LOCK_PATH: &str = "lock.toml";
const REQUIREMENTS_PATH: &str = "requirements.toml";
const OPTIONS_PATH: &str = "metadata/export-options.toml";
const OBSERVATIONS_PATH: &str = "metadata/observations.toml";
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
        if input.container() != ContainerKind::Zip || !has_entry(input, MANIFEST_PATH) {
            return Ok(no_match());
        }
        let bytes = read_entry(input, MANIFEST_PATH)?;
        let Ok(manifest) = parse_toml::<NativeManifest>(&bytes) else {
            return Ok(no_match());
        };
        Ok(PackProbe {
            confidence: u8::from(manifest.schema == SCHEMA && manifest.format == CODEC_ID) * 100,
            reason: Some("native manifest".to_owned()),
        })
    }

    fn plan_import(
        &self,
        input: &dyn PackInput,
        _: &PackImportContext,
        _: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        if input.container() != ContainerKind::Zip {
            return Err(PackCodecError::FormatMismatch);
        }
        let manifest_bytes = read_entry(input, MANIFEST_PATH)?;
        let manifest: NativeManifest = parse_toml(&manifest_bytes)?;
        if manifest.schema != SCHEMA || manifest.format != CODEC_ID {
            return Err(PackCodecError::FormatMismatch);
        }
        let lockfile: Lockfile = parse_toml(&read_entry(input, &manifest.lockfile)?)?;
        if manifest.compatibility.plan_id != lockfile.plan.id
            || manifest.compatibility.lock_schema != lockfile.schema
        {
            return Err(PackCodecError::Codec(
                "native manifest does not describe its lockfile".to_owned(),
            ));
        }
        let requirements: NativeRequirements =
            parse_toml(&read_entry(input, &manifest.requirements)?)?;
        if requirements.schema != SCHEMA {
            return Err(PackCodecError::Codec(format!(
                "unsupported native requirements schema {}",
                requirements.schema
            )));
        }
        let required = lockfile.required_blobs();
        if let Some(stray) = requirements
            .requirement
            .iter()
            .filter_map(|requirement| requirement.digest)
            .find(|digest| !required.contains(digest))
        {
            return Err(PackCodecError::Codec(format!(
                "requirement {stray} is not referenced by the lockfile"
            )));
        }
        let present: BTreeSet<&RelPath> = input.entries().iter().map(|entry| &entry.path).collect();
        let mut destinations: BTreeMap<Digest, RelPath> = BTreeMap::new();
        for (path, digest) in &lockfile.deployment {
            destinations.entry(*digest).or_insert_with(|| path.clone());
        }
        let mut embedded = Vec::new();
        for digest in required {
            let entry = blob_entry(&digest)?;
            if present.contains(&entry) {
                embedded.push(EmbeddedBlob {
                    entry,
                    digest,
                    destination: destinations.get(&digest).cloned(),
                });
            }
        }
        Ok(PackImportPlan {
            codec: CODEC_ID.to_owned(),
            title: None,
            origin: PackOrigin {
                codec: CODEC_ID.to_owned(),
                pack: None,
                version: None,
                digest: Digest::of_bytes(&manifest_bytes),
            },
            target: ImportedTarget {
                game: Some(lockfile.plan.id.clone()),
                game_version: lockfile.target.game_version.clone(),
                loader: Some(lockfile.target.loader.clone()),
                loader_version: lockfile.target.loader_version.clone(),
            },
            lockfile: Some(lockfile),
            environment: requirements.environment,
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
        let mut seen = BTreeSet::new();
        let embedded: Vec<PackFile> = context
            .files
            .iter()
            .filter(|file| {
                context.inclusion.embed.contains(&file.digest) && seen.insert(file.digest)
            })
            .cloned()
            .collect();
        let mode = match options.get("blob-mode") {
            Some(PackOptionValue::Choice(mode)) => mode.clone(),
            _ => "custom".to_owned(),
        };
        let state = serde_json::to_value(NativeState {
            lock: toml::to_string(context.lockfile).map_err(codec_error)?,
            lock_schema: context.lockfile.schema,
            plan_id: context.game.id.clone(),
            plan_version: context.game.version.clone(),
            plan_digest: context.game.digest,
            fingerprint: context.target.fingerprint.clone(),
            mode,
            observations: context.observations.clone(),
        })
        .map_err(codec_error)?;
        Ok(PackExportPlan {
            codec: CODEC_ID.to_owned(),
            options: options.clone(),
            embedded,
            requirements: context.inclusion.requirements.clone(),
            environment: context.inclusion.environment.clone(),
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
            schema: SCHEMA,
            requirement: &plan.requirements,
            environment: &plan.environment,
        })
        .map_err(codec_error)?;
        let manifest = toml::to_string(&NativeManifest {
            schema: SCHEMA,
            format: CODEC_ID.to_owned(),
            created_by: env!("CARGO_PKG_VERSION").to_owned(),
            lockfile: LOCK_PATH.to_owned(),
            requirements: REQUIREMENTS_PATH.to_owned(),
            content: NativeContent {
                mode: state.mode,
                embedded: plan.embedded.len(),
                referenced: plan.requirements.len(),
            },
            compatibility: NativeCompatibility {
                lock_schema: state.lock_schema,
                plan_id: state.plan_id,
                plan_version: state.plan_version,
                plan_digest: state.plan_digest,
                fingerprint: state.fingerprint,
            },
        })
        .map_err(codec_error)?;

        let mut entries = BTreeMap::from([
            (
                LOCK_PATH.to_owned(),
                EntryContent::Inline(state.lock.into_bytes()),
            ),
            (
                MANIFEST_PATH.to_owned(),
                EntryContent::Inline(manifest.into_bytes()),
            ),
            (
                OPTIONS_PATH.to_owned(),
                EntryContent::Inline(
                    toml::to_string(&plan.options)
                        .map_err(codec_error)?
                        .into_bytes(),
                ),
            ),
            (
                REQUIREMENTS_PATH.to_owned(),
                EntryContent::Inline(requirements.into_bytes()),
            ),
        ]);
        if !state.observations.entries.is_empty() {
            entries.insert(
                OBSERVATIONS_PATH.to_owned(),
                EntryContent::Inline(
                    toml::to_string(&state.observations)
                        .map_err(codec_error)?
                        .into_bytes(),
                ),
            );
        }
        for file in &plan.embedded {
            entries
                .entry(blob_path(&file.digest)?)
                .or_insert(EntryContent::Blob(file.digest));
        }
        Ok(PackLayout {
            container: ContainerKind::Zip,
            entries: entries
                .into_iter()
                .map(|(path, content)| {
                    Ok(LayoutEntry {
                        path: RelPath::new(&path).map_err(codec_error)?,
                        content,
                    })
                })
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_digest: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<InstallationFingerprint>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeState {
    lock: String,
    lock_schema: u32,
    plan_id: String,
    plan_version: String,
    plan_digest: Option<Digest>,
    fingerprint: Option<InstallationFingerprint>,
    mode: String,
    observations: Observations,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRequirements {
    schema: u32,
    #[serde(default)]
    requirement: Vec<PackRequirement>,
    #[serde(default)]
    environment: Vec<EnvironmentRequirement>,
}

#[derive(Serialize)]
struct NativeRequirementsRef<'a> {
    schema: u32,
    #[serde(skip_serializing_if = "<[PackRequirement]>::is_empty")]
    requirement: &'a [PackRequirement],
    #[serde(skip_serializing_if = "<[EnvironmentRequirement]>::is_empty")]
    environment: &'a [EnvironmentRequirement],
}

fn no_match() -> PackProbe {
    PackProbe {
        confidence: 0,
        reason: None,
    }
}

fn has_entry(input: &dyn PackInput, path: &str) -> bool {
    input
        .entries()
        .iter()
        .any(|entry| entry.path.as_str() == path)
}

fn read_entry(input: &dyn PackInput, path: &str) -> Result<Vec<u8>, PackCodecError> {
    input.read(
        &RelPath::new(path).map_err(|error| PackCodecError::UnsafePath(error.to_string()))?,
        DOCUMENT_LIMIT,
    )
}

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
    let prefix = hex.get(..2).unwrap_or_default();
    let remainder = hex.get(2..).unwrap_or_default();
    Ok(format!("blobs/sha256/{prefix}/{remainder}"))
}

fn blob_entry(digest: &Digest) -> Result<RelPath, PackCodecError> {
    RelPath::new(&blob_path(digest)?).map_err(codec_error)
}

fn codec_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use msbe_core::instance::{
        BlobSource, DistributionDecision, InstallAnswers, LockedFileClassification, LockedModule,
        LockedPlan, LockedTarget, Lockfile, Name, PackFileRole, ProfileLayer, StoredFile,
    };
    use msbe_fsops::{Digest, RelPath};
    use msbe_plan_schema::Side;
    use msbe_provider_api::{
        ContainerKind, EntryContent, EnvironmentRequirement, Observations, PackCodec as _,
        PackCodecError, PackEntry, PackExportContext, PackFile, PackImportContext, PackInclusion,
        PackInput, PackLayout, PackOptions,
    };

    use super::NativeCodec;

    /// A bundle held in memory, read back through the host contract.
    struct MemoryInput {
        entries: Vec<PackEntry>,
        bytes: BTreeMap<RelPath, Vec<u8>>,
    }

    impl MemoryInput {
        fn from_layout(layout: &PackLayout, blobs: &BTreeMap<Digest, Vec<u8>>) -> Self {
            let bytes: BTreeMap<RelPath, Vec<u8>> = layout
                .entries
                .iter()
                .map(|entry| {
                    let content = match &entry.content {
                        EntryContent::Inline(bytes) => bytes.clone(),
                        EntryContent::Blob(digest) => blobs[digest].clone(),
                    };
                    (entry.path.clone(), content)
                })
                .collect();
            Self {
                entries: bytes
                    .iter()
                    .map(|(path, content)| PackEntry {
                        path: path.clone(),
                        size: content.len() as u64,
                    })
                    .collect(),
                bytes,
            }
        }
    }

    impl PackInput for MemoryInput {
        fn container(&self) -> ContainerKind {
            ContainerKind::Zip
        }

        fn entries(&self) -> &[PackEntry] {
            &self.entries
        }

        fn read(&self, path: &RelPath, _: u64) -> Result<Vec<u8>, PackCodecError> {
            self.bytes
                .get(path)
                .cloned()
                .ok_or(PackCodecError::FormatMismatch)
        }
    }

    fn path(raw: &str) -> RelPath {
        RelPath::new(raw).unwrap()
    }

    fn layered_lockfile(config: Digest, local: Digest) -> Lockfile {
        let module = LockedModule {
            origin: "local.jar".to_owned(),
            provider: None,
            files: vec![StoredFile {
                source: path("local.jar"),
                blob: local,
            }],
            layer: "pack".to_owned(),
            answers: InstallAnswers::from([(
                "fomod".to_owned(),
                BTreeMap::from([("variant".to_owned(), "lite".to_owned())]),
            )]),
        };
        Lockfile {
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
            mods: BTreeMap::from([(Name::new("local").unwrap(), module)]),
            components: BTreeMap::new(),
            deployment: BTreeMap::from([
                (path("config/example.toml"), config),
                (path("mods/local.jar"), local),
            ]),
            classifications: BTreeMap::from([
                (
                    path("config/example.toml"),
                    LockedFileClassification {
                        role: PackFileRole::PackOwnedConfig,
                        source: BlobSource::PackOwned,
                    },
                ),
                (
                    path("mods/local.jar"),
                    LockedFileClassification {
                        role: PackFileRole::LocalArtifact,
                        source: BlobSource::Local,
                    },
                ),
            ]),
            layers: vec![ProfileLayer {
                id: "pack".to_owned(),
                kind: "pack".to_owned(),
                codec: Some("msbe-native".to_owned()),
                pack: Some("example".to_owned()),
                version: Some("1".to_owned()),
                digest: None,
                mods: BTreeMap::new(),
                configs: BTreeMap::new(),
                order: Vec::new(),
            }],
        }
    }

    #[test]
    fn bundles_are_deterministic_and_round_trip_the_lockfile() -> Result<(), PackCodecError> {
        let codec = NativeCodec::new();
        let config_bytes = b"pack-owned = true\n".to_vec();
        let local_bytes = b"local jar".to_vec();
        let config = Digest::of_bytes(&config_bytes);
        let local = Digest::of_bytes(&local_bytes);
        let lockfile = layered_lockfile(config, local);
        let files = [
            PackFile {
                path: path("config/example.toml"),
                digest: config,
                role: PackFileRole::PackOwnedConfig,
                layer: "pack".to_owned(),
                deployed: true,
                source: BlobSource::PackOwned,
                distribution: DistributionDecision::Allowed,
            },
            PackFile {
                path: path("mods/local.jar"),
                digest: local,
                role: PackFileRole::LocalArtifact,
                layer: "pack".to_owned(),
                deployed: true,
                source: BlobSource::Local,
                distribution: DistributionDecision::Unknown,
            },
        ];
        let environment = EnvironmentRequirement {
            root: "game".to_owned(),
            path: path("versions/1.5.2/1.5.2.jar"),
            digest: Digest::of_bytes(b"vanilla"),
        };
        let inclusion = PackInclusion {
            embed: BTreeSet::from([config]),
            permitted: BTreeSet::from([config, local]),
            requirements: Vec::new(),
            environment: vec![environment.clone()],
        };
        let context = PackExportContext {
            game: &lockfile.plan,
            target: &lockfile.target,
            lockfile: &lockfile,
            files: &files,
            observations: &Observations::default(),
            inclusion: &inclusion,
        };
        let plan = codec.plan_export(&context, &PackOptions::new())?;
        assert_eq!(
            plan.embedded
                .iter()
                .map(|file| file.digest)
                .collect::<Vec<_>>(),
            [config],
            "only host-permitted blobs are embedded"
        );
        let first = codec.layout(&plan)?;
        assert_eq!(first, codec.layout(&plan)?);
        let names: Vec<&str> = first
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);

        let blobs = BTreeMap::from([(config, config_bytes), (local, local_bytes)]);
        let input = MemoryInput::from_layout(&first, &blobs);
        assert_eq!(codec.probe(&input)?.confidence, 100);
        let imported = codec.plan_import(
            &input,
            &PackImportContext {
                game: None,
                target: None,
            },
            &PackOptions::new(),
        )?;
        assert_eq!(imported.lockfile.as_ref(), Some(&lockfile));
        assert_eq!(imported.environment, [environment]);
        assert_eq!(
            imported
                .embedded
                .iter()
                .map(|blob| (blob.digest, blob.destination.clone()))
                .collect::<Vec<_>>(),
            [(config, Some(path("config/example.toml")))]
        );
        Ok(())
    }
}
