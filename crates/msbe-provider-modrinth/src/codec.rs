//! Modrinth `.mrpack` pack codec.

use std::collections::BTreeMap;

use msbe_fsops::RelPath;
use msbe_provider_api::{
    Availability, ContainerKind, EntryContent, ImportedTarget, LayoutEntry, PackCodec,
    PackCodecDescriptor, PackCodecError, PackDirections, PackExportContext, PackExportPlan,
    PackFile, PackImportContext, PackImportPlan, PackInput, PackLayout, PackOptionSchema,
    PackOptions, PackOrigin, PackProbe, PackRequirement, PackWarning, RequirementSource,
    SupportSet,
};
use serde::{Deserialize, Serialize};

const CODEC_ID: &str = "modrinth-mrpack";
const INDEX_PATH: &str = "modrinth.index.json";
const DOCUMENT_LIMIT: u64 = 16 << 20;

/// Modrinth's portable `.mrpack` format.
#[derive(Debug)]
pub(crate) struct ModrinthCodec {
    descriptor: PackCodecDescriptor,
}

impl ModrinthCodec {
    /// Creates the schema-1 Modrinth pack codec.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            descriptor: PackCodecDescriptor {
                id: CODEC_ID.to_owned(),
                provider: Some(crate::ID.to_owned()),
                name: "Modrinth modpack".to_owned(),
                extensions: vec!["mrpack".to_owned()],
                media_types: vec!["application/x-modrinth-modpack+zip".to_owned()],
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

impl Default for ModrinthCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl PackCodec for ModrinthCodec {
    fn descriptor(&self) -> &PackCodecDescriptor {
        &self.descriptor
    }

    fn probe(&self, input: &dyn PackInput) -> Result<PackProbe, PackCodecError> {
        if input.container() != ContainerKind::Zip {
            return Ok(no_match());
        }
        let index = match read_index(input) {
            Ok(index) => index,
            Err(PackCodecError::FormatMismatch) => return Ok(no_match()),
            Err(error) => return Err(error),
        };
        Ok(PackProbe {
            confidence: u8::from(index.format_version == 1) * 100,
            reason: Some("Modrinth pack index".to_owned()),
        })
    }

    fn plan_import(
        &self,
        input: &dyn PackInput,
        context: &PackImportContext,
        _: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        if input.container() != ContainerKind::Zip {
            return Err(PackCodecError::FormatMismatch);
        }
        let index_bytes = input.read(&index_path()?, DOCUMENT_LIMIT)?;
        let index: Index = parse_index(&index_bytes)?;
        if index.format_version != 1 {
            return Err(PackCodecError::Codec(format!(
                "unsupported Modrinth pack format version {}",
                index.format_version
            )));
        }
        let requirements = index
            .files
            .into_iter()
            .filter_map(|file| {
                let side = match context.target.as_ref().map(|target| target.side) {
                    Some(msbe_plan_schema::Side::Client) => file.env.client.availability(),
                    Some(msbe_plan_schema::Side::Server) => file.env.server.availability(),
                    None => Availability::Required,
                };
                (side != Availability::Unsupported).then_some(PackRequirement {
                    digest: None,
                    hashes: file.hashes,
                    destination: Some(file.path),
                    side,
                    answers: BTreeMap::default(),
                    sources: vec![RequirementSource::Direct {
                        urls: file.downloads,
                    }],
                })
            })
            .collect();
        Ok(PackImportPlan {
            codec: CODEC_ID.to_owned(),
            title: index.name,
            origin: PackOrigin {
                codec: CODEC_ID.to_owned(),
                pack: None,
                version: Some(index.version_id),
                digest: msbe_fsops::Digest::of_bytes(&index_bytes),
            },
            lockfile: None,
            target: ImportedTarget::default(),
            environment: Vec::new(),
            requirements,
            embedded: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn plan_export(
        &self,
        context: &PackExportContext<'_>,
        options: &PackOptions,
    ) -> Result<PackExportPlan, PackCodecError> {
        let game_version = context.target.game_version.clone().ok_or_else(|| {
            PackCodecError::UnsupportedTarget {
                game: context.game.id.clone(),
                loader: context.target.loader.clone(),
            }
        })?;
        if !context.inclusion.environment.is_empty() {
            return Err(PackCodecError::Unreproducible(
                "Modrinth packs cannot declare installation-owned inputs".to_owned(),
            ));
        }
        let mut dependencies = BTreeMap::from([("minecraft".to_owned(), game_version)]);
        if let Some(loader_version) = &context.target.loader_version {
            dependencies.insert(
                loader_dependency(&context.target.loader),
                loader_version.clone(),
            );
        }
        // An index can only reference direct downloads. A requirement without one is embedded as
        // an override instead, which the host allows only for digests policy permits embedding.
        let mut embed = context.inclusion.embed.clone();
        let mut files = Vec::new();
        let mut requirements = Vec::new();
        let mut warnings = Vec::new();
        for requirement in &context.inclusion.requirements {
            match index_file(requirement) {
                Ok(file) => {
                    files.push(file);
                    requirements.push(requirement.clone());
                }
                Err(error) => {
                    let Some(digest) = requirement
                        .digest
                        .filter(|digest| context.inclusion.permitted.contains(digest))
                        .filter(|_| requirement.destination.is_some())
                    else {
                        return Err(error);
                    };
                    embed.insert(digest);
                    warnings.push(PackWarning {
                        code: "embedded-reference".to_owned(),
                        message: format!("{error}; it is embedded as an override instead"),
                    });
                }
            }
        }
        let state = serde_json::to_value(ExportState {
            name: None,
            dependencies,
            files,
        })
        .map_err(codec_error)?;
        Ok(PackExportPlan {
            codec: CODEC_ID.to_owned(),
            options: options.clone(),
            embedded: context
                .files
                .iter()
                .filter(|file| file.deployed && embed.contains(&file.digest))
                .cloned()
                .collect(),
            requirements,
            environment: Vec::new(),
            warnings,
            codec_state: state,
        })
    }

    fn layout(&self, plan: &PackExportPlan) -> Result<PackLayout, PackCodecError> {
        if plan.codec != CODEC_ID {
            return Err(PackCodecError::Codec(
                "export plan belongs to another codec".to_owned(),
            ));
        }
        let state: ExportState =
            serde_json::from_value(plan.codec_state.clone()).map_err(codec_error)?;
        let index = serde_json::to_vec_pretty(&ExportIndex {
            format_version: 1,
            game: "minecraft",
            version_id: "1.0.0",
            name: state.name.as_deref(),
            files: state
                .files
                .iter()
                .map(|file| ExportIndexFile {
                    path: &file.path,
                    hashes: &file.hashes,
                    env: ExportEnvironment {
                        client: "required",
                        server: "required",
                    },
                    downloads: &file.downloads,
                })
                .collect(),
            dependencies: state.dependencies,
        })
        .map_err(codec_error)?;
        let mut entries = BTreeMap::from([(INDEX_PATH.to_owned(), EntryContent::Inline(index))]);
        for PackFile { path, digest, .. } in &plan.embedded {
            entries.insert(format!("overrides/{path}"), EntryContent::Blob(*digest));
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

fn read_index(input: &dyn PackInput) -> Result<Index, PackCodecError> {
    let path = index_path()?;
    if !input.entries().iter().any(|entry| entry.path == path) {
        return Err(PackCodecError::FormatMismatch);
    }
    parse_index(&input.read(&path, DOCUMENT_LIMIT)?)
}

fn index_path() -> Result<RelPath, PackCodecError> {
    RelPath::new(INDEX_PATH).map_err(codec_error)
}

fn parse_index(bytes: &[u8]) -> Result<Index, PackCodecError> {
    serde_json::from_slice(bytes).map_err(codec_error)
}

fn no_match() -> PackProbe {
    PackProbe {
        confidence: 0,
        reason: None,
    }
}

/// A referenced file as an index entry. Modrinth indexes can only name direct HTTPS downloads.
fn index_file(requirement: &PackRequirement) -> Result<ExportFile, PackCodecError> {
    let path = requirement.destination.clone().ok_or_else(|| {
        PackCodecError::Unreproducible("a referenced file has no deployment path".to_owned())
    })?;
    let downloads: Vec<String> = requirement
        .sources
        .iter()
        .filter_map(|source| match source {
            RequirementSource::Direct { urls } => Some(urls),
            RequirementSource::Provider { .. } | RequirementSource::UserAction { .. } => None,
        })
        .flatten()
        .filter(|url| url.starts_with("https://"))
        .cloned()
        .collect();
    if downloads.is_empty() {
        return Err(PackCodecError::Unreproducible(format!(
            "{path} has no direct HTTPS download a Modrinth index can reference; embed it with blob-mode complete"
        )));
    }
    Ok(ExportFile {
        path,
        hashes: requirement.hashes.clone(),
        downloads,
    })
}

fn loader_dependency(loader: &str) -> String {
    match loader {
        "fabric" => "fabric-loader".to_owned(),
        "quilt" => "quilt-loader".to_owned(),
        _ => loader.to_owned(),
    }
}

fn codec_error(error: impl std::fmt::Display) -> PackCodecError {
    PackCodecError::Codec(error.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    format_version: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_version_id")]
    version_id: String,
    #[serde(default)]
    files: Vec<IndexFile>,
}

fn default_version_id() -> String {
    "1.0.0".to_owned()
}

#[derive(Deserialize)]
struct IndexFile {
    path: RelPath,
    downloads: Vec<String>,
    hashes: BTreeMap<String, String>,
    #[serde(default)]
    env: Environment,
}

#[derive(Default, Deserialize)]
struct Environment {
    #[serde(default)]
    client: EnvironmentRequirement,
    #[serde(default)]
    server: EnvironmentRequirement,
}

#[derive(Deserialize)]
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
    const fn availability(&self) -> Availability {
        match self {
            Self::Boolean(true) | Self::State(EnvironmentState::Required) => Availability::Required,
            Self::Boolean(false) | Self::State(EnvironmentState::Unsupported) => {
                Availability::Unsupported
            }
            Self::State(EnvironmentState::Optional) => Availability::Optional,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum EnvironmentState {
    Required,
    Optional,
    Unsupported,
}

#[derive(Serialize, Deserialize)]
struct ExportState {
    name: Option<String>,
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    files: Vec<ExportFile>,
}

#[derive(Serialize, Deserialize)]
struct ExportFile {
    path: RelPath,
    hashes: BTreeMap<String, String>,
    downloads: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportIndex<'a> {
    format_version: u32,
    game: &'static str,
    version_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    files: Vec<ExportIndexFile<'a>>,
    dependencies: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ExportIndexFile<'a> {
    path: &'a RelPath,
    hashes: &'a BTreeMap<String, String>,
    env: ExportEnvironment,
    downloads: &'a [String],
}

#[derive(Serialize)]
struct ExportEnvironment {
    client: &'static str,
    server: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use msbe_provider_api::{PackEntry, PackInput};

    #[derive(Debug)]
    struct Input {
        entries: Vec<PackEntry>,
        index: Vec<u8>,
    }

    impl PackInput for Input {
        fn container(&self) -> ContainerKind {
            ContainerKind::Zip
        }
        fn entries(&self) -> &[PackEntry] {
            &self.entries
        }
        fn read(&self, path: &RelPath, _: u64) -> Result<Vec<u8>, PackCodecError> {
            if path.as_str() == INDEX_PATH {
                Ok(self.index.clone())
            } else {
                Err(PackCodecError::FormatMismatch)
            }
        }
    }

    #[test]
    fn import_maps_downloads_hashes_and_environment() -> Result<(), PackCodecError> {
        let index = br#"{"formatVersion":1,"files":[{"path":"mods/example.jar","downloads":["https://example.test/example.jar"],"hashes":{"sha512":"abc"},"env":{"client":"required","server":"unsupported"}}]}"#.to_vec();
        let input = Input {
            entries: vec![PackEntry {
                path: index_path()?,
                size: index.len() as u64,
            }],
            index,
        };
        let plan = ModrinthCodec::new().plan_import(
            &input,
            &PackImportContext {
                game: None,
                target: None,
            },
            &PackOptions::new(),
        )?;
        let [requirement] = plan.requirements.as_slice() else {
            panic!("expected one requirement, got {:?}", plan.requirements);
        };
        assert_eq!(requirement.side, Availability::Required);
        assert_eq!(
            requirement.hashes.get("sha512").map(String::as_str),
            Some("abc")
        );
        Ok(())
    }

    /// An empty lockfile for a Fabric client on 1.21.1.
    fn fabric_lockfile() -> msbe_core::instance::Lockfile {
        use msbe_core::instance::{LockedPlan, LockedTarget, Lockfile};

        Lockfile {
            schema: 2,
            plan: LockedPlan {
                id: "minecraft".to_owned(),
                version: "0.2.0".to_owned(),
                digest: None,
            },
            target: LockedTarget {
                game_version: Some("1.21.1".to_owned()),
                loader: "fabric".to_owned(),
                loader_version: Some("0.16.10".to_owned()),
                side: msbe_plan_schema::Side::Client,
                fingerprint: None,
            },
            order: Vec::new(),
            mods: BTreeMap::new(),
            components: BTreeMap::new(),
            deployment: BTreeMap::new(),
            classifications: BTreeMap::new(),
            layers: Vec::new(),
        }
    }

    #[test]
    fn export_references_direct_downloads_and_embeds_only_permitted_files()
    -> Result<(), PackCodecError> {
        use std::collections::BTreeSet;

        use msbe_fsops::Digest;
        use msbe_provider_api::{
            BlobSource, DistributionDecision, Observations, PackExportContext, PackFile,
            PackFileRole, PackInclusion,
        };

        let path = |raw: &str| RelPath::new(raw).map_err(codec_error);
        let config = Digest::of_bytes(b"config");
        let local = Digest::of_bytes(b"local");
        let referenced = Digest::of_bytes(b"referenced");
        let lockfile = fabric_lockfile();
        let file = |raw: &str, digest, role, source| -> Result<PackFile, PackCodecError> {
            Ok(PackFile {
                path: path(raw)?,
                digest,
                role,
                layer: "changes".to_owned(),
                deployed: true,
                source,
                distribution: DistributionDecision::Unknown,
            })
        };
        let files = [
            file(
                "config/a.toml",
                config,
                PackFileRole::PackOwnedConfig,
                BlobSource::PackOwned,
            )?,
            file(
                "mods/local.jar",
                local,
                PackFileRole::LocalArtifact,
                BlobSource::Local,
            )?,
        ];
        let inclusion = PackInclusion {
            embed: BTreeSet::from([config]),
            permitted: BTreeSet::from([config, local]),
            requirements: vec![PackRequirement {
                digest: Some(referenced),
                hashes: BTreeMap::from([("sha512".to_owned(), "abc".to_owned())]),
                destination: Some(path("mods/referenced.jar")?),
                side: Availability::Required,
                answers: BTreeMap::default(),
                sources: vec![RequirementSource::Direct {
                    urls: vec!["https://cdn.example.test/referenced.jar".to_owned()],
                }],
            }],
            environment: Vec::new(),
        };
        let codec = ModrinthCodec::new();
        let plan = codec.plan_export(
            &PackExportContext {
                game: &lockfile.plan,
                target: &lockfile.target,
                lockfile: &lockfile,
                files: &files,
                observations: &Observations::default(),
                inclusion: &inclusion,
            },
            &PackOptions::new(),
        )?;
        let layout = codec.layout(&plan)?;
        let paths: Vec<&str> = layout
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, ["modrinth.index.json", "overrides/config/a.toml"]);
        let Some(EntryContent::Inline(index)) = layout.entries.first().map(|entry| &entry.content)
        else {
            panic!("the index is generated inline");
        };
        let index: serde_json::Value = serde_json::from_slice(index).map_err(codec_error)?;
        let text = |pointer: &str| index.pointer(pointer).and_then(serde_json::Value::as_str);
        assert_eq!(text("/files/0/path"), Some("mods/referenced.jar"));
        assert_eq!(
            text("/files/0/downloads/0"),
            Some("https://cdn.example.test/referenced.jar")
        );
        assert_eq!(text("/dependencies/fabric-loader"), Some("0.16.10"));
        Ok(())
    }

    #[test]
    fn probe_rejects_a_non_modrinth_zip() -> Result<(), PackCodecError> {
        let input = Input {
            entries: Vec::new(),
            index: Vec::new(),
        };
        assert_eq!(ModrinthCodec::new().probe(&input)?.confidence, 0);
        Ok(())
    }
}
