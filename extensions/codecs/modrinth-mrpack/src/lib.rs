//! Modrinth's `.mrpack` format, served from MSBE's WebAssembly sandbox.
//!
//! A `.mrpack` is a ZIP holding `modrinth.index.json`, which lists files the recipient downloads
//! from HTTPS URLs, and `overrides/`, copied into the instance as they are. Import maps the index
//! to neutral requirements, keeping each file's side availability. Export writes an index for the
//! requirements that have a direct HTTPS download, and embeds what the host permits as overrides.
//!
//! The codec is bound to the `modrinth` provider, so the host serves it under that provider's
//! policy. Its conformance suite and golden transcript are in `conformance/`: any change to what
//! the codec produces shows up there as a diff.

use std::collections::{BTreeMap, BTreeSet};

use msbe_codec_guest::{Codec, Container, Error, Input, Value, json, serde_json};
use serde::{Deserialize, Deserializer, Serialize};

const CODEC_ID: &str = "modrinth-mrpack";
const PROVIDER: &str = "modrinth";
const INDEX_PATH: &str = "modrinth.index.json";
const DOCUMENT_LIMIT: u64 = 16 << 20;
/// The host replaces an import origin's digest with the digest of the whole input.
const HOST_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// The `.mrpack` codec.
#[derive(Debug)]
pub struct ModrinthPack;

impl Codec for ModrinthPack {
    fn descriptor() -> Value {
        json!({
            "id": CODEC_ID,
            "provider": PROVIDER,
            "name": "Modrinth modpack",
            "extensions": ["mrpack"],
            "media_types": ["application/x-modrinth-modpack+zip"],
            "directions": { "import": true, "export": true },
            "supported_games": { "kind": "universal" },
            "option_schema": { "schema": 1 }
        })
    }

    fn probe(input: &Input) -> Result<Value, Error> {
        if input.container() != Container::Zip {
            return Ok(no_match());
        }
        let index = match read_index(input) {
            Ok(index) => index,
            Err(Error::FormatMismatch) => return Ok(no_match()),
            Err(error) => return Err(error),
        };
        Ok(json!({
            "confidence": if index.format_version == 1 { 100 } else { 0 },
            "reason": "Modrinth pack index"
        }))
    }

    fn plan_import(input: &Input, request: Value) -> Result<Value, Error> {
        if input.container() != Container::Zip {
            return Err(Error::FormatMismatch);
        }
        let index = parse_index(&input.read(INDEX_PATH, DOCUMENT_LIMIT)?)?;
        if index.format_version != 1 {
            return Err(Error::codec(format!(
                "unsupported Modrinth pack format version {}",
                index.format_version
            )));
        }
        let side = request["context"]["target"]["side"].as_str();
        let requirements: Vec<Value> = index
            .files
            .into_iter()
            .filter_map(|file| {
                let availability = match side {
                    Some("client") => file.env.client.availability(),
                    Some("server") => file.env.server.availability(),
                    _ => "required",
                };
                (availability != "unsupported").then(|| {
                    json!({
                        "digest": null,
                        "hashes": file.hashes,
                        "destination": file.path.0,
                        "side": availability,
                        "answers": {},
                        "sources": [{ "kind": "direct", "urls": file.downloads }]
                    })
                })
            })
            .collect();
        Ok(json!({
            "codec": CODEC_ID,
            "title": index.name,
            "origin": {
                "codec": CODEC_ID,
                "pack": null,
                "version": index.version_id,
                "digest": HOST_DIGEST
            },
            "target": { "game": null, "game_version": null, "loader": null, "loader_version": null },
            "requirements": requirements,
            "embedded": [],
            "warnings": []
        }))
    }

    fn plan_export(request: Value) -> Result<Value, Error> {
        let context = &request["context"];
        let target = &context["target"];
        let Some(game_version) = target["game_version"].as_str() else {
            return Err(Error::UnsupportedTarget {
                game: text(&context["game"]["id"]),
                loader: text(&target["loader"]),
            });
        };
        let inclusion = &context["inclusion"];
        if items(&inclusion["environment"]).next().is_some() {
            return Err(Error::Unreproducible {
                message: "Modrinth packs cannot declare installation-owned inputs".to_owned(),
            });
        }
        let mut dependencies = BTreeMap::from([("minecraft".to_owned(), game_version.to_owned())]);
        if let Some(loader_version) = target["loader_version"].as_str() {
            dependencies.insert(
                loader_dependency(&text(&target["loader"])),
                loader_version.to_owned(),
            );
        }
        // An index can only reference direct downloads. A requirement without one is embedded as an
        // override instead, which the host allows only for digests policy permits embedding.
        let mut embed: BTreeSet<&str> = items(&inclusion["embed"]).filter_map(Value::as_str).collect();
        let permitted: BTreeSet<&str> =
            items(&inclusion["permitted"]).filter_map(Value::as_str).collect();
        let mut files = Vec::new();
        let mut requirements = Vec::new();
        let mut warnings = Vec::new();
        for requirement in items(&inclusion["requirements"]) {
            match index_file(requirement) {
                Ok(file) => {
                    files.push(file);
                    requirements.push(requirement.clone());
                }
                Err(message) => {
                    let Some(digest) = requirement["digest"]
                        .as_str()
                        .filter(|digest| permitted.contains(digest))
                        .filter(|_| !requirement["destination"].is_null())
                    else {
                        return Err(Error::Unreproducible { message });
                    };
                    embed.insert(digest);
                    warnings.push(json!({
                        "code": "embedded-reference",
                        "message": format!(
                            "pack content is not reproducible: {message}; it is embedded as an override instead"
                        )
                    }));
                }
            }
        }
        let embedded: Vec<Value> = items(&context["files"])
            .filter(|file| {
                file["deployed"] == Value::Bool(true)
                    && file["digest"].as_str().is_some_and(|digest| embed.contains(digest))
            })
            .cloned()
            .collect();
        let state = ExportState {
            name: None,
            dependencies,
            files,
        };
        Ok(json!({
            "codec": CODEC_ID,
            "options": request["options"],
            "embedded": embedded,
            "requirements": requirements,
            "environment": [],
            "warnings": warnings,
            "codec_state": serde_json::to_value(&state).map_err(Error::codec)?
        }))
    }

    fn layout(plan: Value) -> Result<Value, Error> {
        if plan["codec"] != CODEC_ID {
            return Err(Error::codec("export plan belongs to another codec"));
        }
        let state: ExportState =
            serde_json::from_value(plan["codec_state"].clone()).map_err(Error::codec)?;
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
        .map_err(Error::codec)?;
        let mut entries =
            BTreeMap::from([(INDEX_PATH.to_owned(), json!({ "kind": "inline", "value": index }))]);
        for file in items(&plan["embedded"]) {
            let (Some(path), Some(digest)) = (file["path"].as_str(), file["digest"].as_str()) else {
                return Err(Error::codec("an embedded file has no path or digest"));
            };
            entries.insert(
                format!("overrides/{path}"),
                json!({ "kind": "blob", "value": digest }),
            );
        }
        let entries: Vec<Value> = entries
            .into_iter()
            .map(|(path, content)| json!({ "path": path, "content": content }))
            .collect();
        Ok(json!({ "container": "zip", "entries": entries }))
    }
}

fn read_index(input: &Input) -> Result<Index, Error> {
    if !input.contains(INDEX_PATH)? {
        return Err(Error::FormatMismatch);
    }
    parse_index(&input.read(INDEX_PATH, DOCUMENT_LIMIT)?)
}

fn parse_index(bytes: &[u8]) -> Result<Index, Error> {
    serde_json::from_slice(bytes).map_err(Error::codec)
}

fn no_match() -> Value {
    json!({ "confidence": 0, "reason": null })
}

/// A referenced requirement as an index entry, or why an index cannot reference it. Modrinth
/// indexes can only name direct HTTPS downloads.
fn index_file(requirement: &Value) -> Result<ExportFile, String> {
    let path = requirement["destination"]
        .as_str()
        .ok_or_else(|| "a referenced file has no deployment path".to_owned())?;
    let downloads: Vec<String> = items(&requirement["sources"])
        .filter(|source| source["kind"] == "direct")
        .flat_map(|source| items(&source["urls"]))
        .filter_map(Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .map(str::to_owned)
        .collect();
    if downloads.is_empty() {
        return Err(format!(
            "{path} has no direct HTTPS download a Modrinth index can reference; embed it with blob-mode complete"
        ));
    }
    let hashes = requirement["hashes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(algorithm, value)| Some((algorithm.clone(), value.as_str()?.to_owned())))
        .collect();
    Ok(ExportFile {
        path: path.to_owned(),
        hashes,
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

fn items(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
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
    path: SafePath,
    downloads: Vec<String>,
    hashes: BTreeMap<String, String>,
    #[serde(default)]
    env: Environment,
}

/// A relative path under the host's rules, refused with the host's message while the index is
/// parsed, so an unsafe index fails where and how the host's own path type would fail it.
struct SafePath(String);

impl<'de> Deserialize<'de> for SafePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        normalize(&raw).map(Self).map_err(serde::de::Error::custom)
    }
}

/// `raw` as a normalized relative path, or the host's reason for refusing it.
fn normalize(raw: &str) -> Result<String, String> {
    let reject = |reason: &str| format!("invalid relative path {raw:?}: {reason}");
    if raw.starts_with('/') {
        return Err(reject("absolute path"));
    }
    if raw.contains('\\') {
        return Err(reject("backslash separator"));
    }
    if raw.contains(':') {
        return Err(reject("colon (drive letter or alternate data stream)"));
    }
    if raw.contains('\0') {
        return Err(reject("NUL byte"));
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" => return Err(reject("empty component")),
            "." => {}
            ".." => return Err(reject("parent component")),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(reject("empty path"));
    }
    Ok(parts.join("/"))
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
    /// The host's side availability this requirement stands for.
    const fn availability(&self) -> &'static str {
        match self {
            Self::Boolean(true) | Self::State(EnvironmentState::Required) => "required",
            Self::Boolean(false) | Self::State(EnvironmentState::Unsupported) => "unsupported",
            Self::State(EnvironmentState::Optional) => "optional",
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
    path: String,
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
    path: &'a str,
    hashes: &'a BTreeMap<String, String>,
    env: ExportEnvironment,
    downloads: &'a [String],
}

#[derive(Serialize)]
struct ExportEnvironment {
    client: &'static str,
    server: &'static str,
}

msbe_codec_guest::export_codec!(ModrinthPack);

#[cfg(test)]
mod tests {
    use msbe_codec_guest::{Codec, Input, json};

    use super::ModrinthPack;

    #[test]
    fn probe_and_import_read_the_index() {
        let index = json!({
            "formatVersion": 1,
            "files": [{
                "path": "mods/a.jar",
                "downloads": ["https://cdn.example.test/a.jar"],
                "hashes": { "sha512": "ab" },
                "env": { "client": "required", "server": "unsupported" }
            }]
        });
        let input = Input::memory([("modrinth.index.json".to_owned(), index.to_string().into_bytes())]);
        assert_eq!(ModrinthPack::probe(&input).unwrap()["confidence"], 100);
        let server = json!({ "context": { "target": { "side": "server" } } });
        let plan = ModrinthPack::plan_import(&input, server).unwrap();
        assert_eq!(plan["requirements"], json!([]));
        assert_eq!(ModrinthPack::probe(&Input::memory([])).unwrap()["confidence"], 0);
    }
}
