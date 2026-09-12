//! Modrinth `.mrpack` pack codec.

use std::collections::BTreeMap;

use msbe_fsops::RelPath;
use msbe_provider_api::{
    Availability, ContainerKind, EntryContent, ImportedTarget, LayoutEntry, PackCodec,
    PackCodecDescriptor, PackCodecError, PackDirections, PackExportContext, PackExportPlan,
    PackFile, PackImportContext, PackImportPlan, PackInput, PackLayout, PackOptionSchema,
    PackOptions, PackOrigin, PackProbe, PackRequirement, RequirementSource, SupportSet,
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
        options: &PackOptions,
    ) -> Result<PackImportPlan, PackCodecError> {
        self.descriptor.option_schema.normalize(options)?;
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
                    answers: Default::default(),
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
        let options = self.descriptor.option_schema.normalize(options)?;
        let game_version = context.target.game_version.clone().ok_or_else(|| {
            PackCodecError::UnsupportedTarget {
                game: context.game.id.clone(),
                loader: context.target.loader.clone(),
            }
        })?;
        let mut dependencies = BTreeMap::from([("minecraft".to_owned(), game_version)]);
        if let Some(loader_version) = &context.target.loader_version {
            dependencies.insert(
                loader_dependency(&context.target.loader),
                loader_version.clone(),
            );
        }
        let state = serde_json::to_value(ExportState {
            name: None,
            dependencies,
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
        let state: ExportState =
            serde_json::from_value(plan.codec_state.clone()).map_err(codec_error)?;
        let index = serde_json::to_vec_pretty(&ExportIndex {
            format_version: 1,
            game: "minecraft",
            version_id: "1.0.0",
            name: state.name.as_deref(),
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportIndex<'a> {
    format_version: u32,
    game: &'static str,
    version_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    dependencies: BTreeMap<String, String>,
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
        assert_eq!(plan.requirements.len(), 1);
        assert_eq!(plan.requirements[0].side, Availability::Required);
        assert_eq!(plan.requirements[0].hashes["sha512"], "abc");
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
