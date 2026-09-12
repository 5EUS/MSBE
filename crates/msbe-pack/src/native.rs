//! Deterministic native `.msbepack` codec.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom, Write},
};

use msbe_archive::Limits;
use msbe_core::instance::Lockfile;
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    BlobReader, EmbeddedBlob, ImportedTarget, PackCodec, PackCodecDescriptor, PackCodecError,
    PackDirections, PackExportContext, PackExportPlan, PackExportResult, PackImportContext,
    PackImportPlan, PackOptionSchema, PackOptions, PackProbe, PackRequirement, ReadSeek,
    SupportSet, WriteSeek,
};
use serde::{Deserialize, Serialize};
use zip::{DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

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

    fn probe(&self, input: &mut dyn ReadSeek) -> Result<PackProbe, PackCodecError> {
        input.seek(SeekFrom::Start(0))?;
        let Ok(mut archive) = ZipArchive::new(input) else {
            return Ok(no_match());
        };
        validate_archive(&mut archive, &Limits::default())?;
        let manifest = match read_entry(&mut archive, MANIFEST_PATH) {
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
        input: &mut dyn ReadSeek,
        _: &PackImportContext,
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        self.descriptor.option_schema.normalize(options)?;
        input.seek(SeekFrom::Start(0))?;
        let mut archive = ZipArchive::new(input).map_err(codec_error)?;
        validate_archive(&mut archive, &Limits::default())?;
        let manifest: NativeManifest = parse_toml(&read_entry(&mut archive, MANIFEST_PATH)?)?;
        if manifest.schema != 1 || manifest.format != CODEC_ID {
            return Err(PackCodecError::FormatMismatch);
        }
        let lockfile: Lockfile = parse_toml(&read_entry(&mut archive, &manifest.lockfile)?)?;
        let requirements: NativeRequirements =
            parse_toml(&read_entry(&mut archive, &manifest.requirements)?)?;
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
                if let Ok(mut blob) = archive.by_name(entry.as_str()) {
                    if Digest::of_reader(&mut blob)? != *digest {
                        return Err(PackCodecError::Codec(format!(
                            "embedded blob {entry:?} does not match {digest}"
                        )));
                    }
                    embedded.push(EmbeddedBlob {
                        entry: RelPath::new(&entry).map_err(|error| {
                            PackCodecError::Codec(format!("invalid native blob path: {error}"))
                        })?,
                        digest: *digest,
                        destination: Some(path.clone()),
                    });
                }
            }
        }
        Ok(PackImportPlan {
            codec: CODEC_ID.to_owned(),
            title: None,
            lockfile: Some(lockfile.clone()),
            target: ImportedTarget {
                game: Some(lockfile.plan.id.clone()),
                game_version: lockfile.target.game_version.clone(),
                loader: Some(lockfile.target.loader.clone()),
                loader_version: lockfile.target.loader_version.clone(),
            },
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

    fn export(
        &self,
        plan: &PackExportPlan,
        blobs: &dyn BlobReader,
        output: &mut dyn WriteSeek,
    ) -> Result<PackExportResult, PackCodecError> {
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
            (LOCK_PATH.to_owned(), Some(state.lock.into_bytes())),
            (MANIFEST_PATH.to_owned(), Some(manifest.into_bytes())),
            (OPTIONS_PATH.to_owned(), Some(options.into_bytes())),
            (
                REQUIREMENTS_PATH.to_owned(),
                Some(requirements.into_bytes()),
            ),
        ]);
        let mut blob_digests = BTreeMap::new();
        for file in &plan.embedded {
            let path = blob_path(&file.digest)?;
            blob_digests.insert(path.clone(), file.digest);
            entries.entry(path).or_insert(None);
        }

        output.seek(SeekFrom::Start(0))?;
        let mut archive = ZipWriter::new(output);
        let file_options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(9))
            .last_modified_time(DateTime::default())
            .unix_permissions(0o644);
        for (path, bytes) in entries {
            archive
                .start_file(&path, file_options)
                .map_err(codec_error)?;
            if let Some(bytes) = bytes {
                archive.write_all(&bytes)?;
            } else if let Some(digest) = blob_digests.get(&path) {
                blobs.copy_blob(digest, &mut archive)?;
            }
        }
        archive.finish().map_err(codec_error)?;
        Ok(PackExportResult {
            codec: CODEC_ID.to_owned(),
            embedded: plan.embedded.len(),
            referenced: plan.requirements.len(),
            digest: None,
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

fn read_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
) -> Result<Vec<u8>, PackCodecError> {
    let entry = archive
        .by_name(path)
        .map_err(|_| PackCodecError::FormatMismatch)?;
    if entry.size() > DOCUMENT_LIMIT {
        return Err(PackCodecError::Limit(format!(
            "{path} exceeds {DOCUMENT_LIMIT} bytes"
        )));
    }
    let mut bytes = Vec::new();
    entry.take(DOCUMENT_LIMIT + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > DOCUMENT_LIMIT {
        return Err(PackCodecError::Limit(format!(
            "{path} exceeds {DOCUMENT_LIMIT} bytes"
        )));
    }
    Ok(bytes)
}

fn validate_archive<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limits: &Limits,
) -> Result<(), PackCodecError> {
    if archive.len() > limits.max_entries {
        return Err(PackCodecError::Limit(format!(
            "archive has more than {} entries",
            limits.max_entries
        )));
    }
    let mut total = 0_u64;
    let mut names = BTreeSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(codec_error)?;
        RelPath::new(entry.name())
            .map_err(|error| PackCodecError::Codec(format!("unsafe native entry: {error}")))?;
        if !names.insert(entry.name().to_ascii_lowercase()) {
            return Err(PackCodecError::Codec(
                "native archive contains duplicate entry names".to_owned(),
            ));
        }
        if entry.size() > limits.max_file_bytes {
            return Err(PackCodecError::Limit(format!(
                "entry {:?} exceeds {} bytes",
                entry.name(),
                limits.max_file_bytes
            )));
        }
        if entry.size() > 1 << 20
            && (entry.compressed_size() == 0
                || entry.compressed_size().saturating_mul(limits.max_ratio) < entry.size())
        {
            return Err(PackCodecError::Limit(format!(
                "entry {:?} exceeds the compression ratio limit",
                entry.name()
            )));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| PackCodecError::Limit("archive size overflow".to_owned()))?;
        if total > limits.max_total_bytes {
            return Err(PackCodecError::Limit(format!(
                "archive exceeds {} total bytes",
                limits.max_total_bytes
            )));
        }
    }
    Ok(())
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
    let (prefix, remainder) = hex.split_at(2);
    Ok(format!("blobs/sha256/{prefix}/{remainder}"))
}

fn codec_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io::Cursor};

    use msbe_core::instance::{
        BlobSource, DistributionDecision, LockedFileClassification, LockedPlan, LockedTarget,
        Lockfile, PackFileRole,
    };
    use msbe_plan_schema::Side;
    use msbe_provider_api::{BlobReader, PackCodec as _, PackExportContext, PackFile};

    use super::{NativeCodec, PackCodecError};

    struct FixtureBlobs(BTreeMap<msbe_fsops::Digest, Vec<u8>>);

    impl BlobReader for FixtureBlobs {
        fn copy_blob(
            &self,
            digest: &msbe_fsops::Digest,
            output: &mut dyn std::io::Write,
        ) -> Result<u64, PackCodecError> {
            let bytes = self
                .0
                .get(digest)
                .ok_or(PackCodecError::MissingBlob(*digest))?;
            output.write_all(bytes)?;
            Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        }
    }

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
            distribution: DistributionDecision::Allowed,
        };
        let lockfile = Lockfile {
            schema: 2,
            plan: LockedPlan {
                id: "example-game".to_owned(),
                version: "1.0.0".to_owned(),
            },
            target: LockedTarget {
                game_version: Some("2.0".to_owned()),
                loader: "example-loader".to_owned(),
                loader_version: Some("3.0".to_owned()),
                side: Side::Client,
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
            distribution: classification.distribution,
        }];
        let context = PackExportContext {
            game: &lockfile.plan,
            target: &lockfile.target,
            lockfile: &lockfile,
            files: &files,
        };
        let plan = codec.plan_export(&context, &BTreeMap::new())?;
        let blobs = FixtureBlobs(BTreeMap::from([(digest, bytes.to_vec())]));

        let mut first = Cursor::new(Vec::new());
        codec.export(&plan, &blobs, &mut first)?;
        let mut second = Cursor::new(Vec::new());
        codec.export(&plan, &blobs, &mut second)?;
        assert_eq!(first.get_ref(), second.get_ref());

        let mut archive =
            zip::ZipArchive::new(Cursor::new(first.get_ref())).map_err(super::codec_error)?;
        let mut names = Vec::new();
        for index in 0..archive.len() {
            names.push(
                archive
                    .by_index(index)
                    .map_err(super::codec_error)?
                    .name()
                    .to_owned(),
            );
        }
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        drop(archive);

        first.set_position(0);
        assert_eq!(codec.probe(&mut first)?.confidence, 100);
        first.set_position(0);
        let imported = codec.plan_import(
            &mut first,
            &msbe_provider_api::PackImportContext {
                game: None,
                target: None,
            },
            &BTreeMap::new(),
        )?;
        assert_eq!(imported.lockfile, Some(lockfile));
        assert_eq!(imported.embedded.len(), 1);
        Ok(())
    }
}
