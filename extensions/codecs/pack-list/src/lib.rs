//! `pack-list`: an example third-party pack format delivered as a sandboxed MSBE codec.
//!
//! A pack-list archive is a ZIP holding `pack-list.json`. Its `files` are downloaded by the
//! recipient from pinned HTTPS URLs and verified by SHA-512; its `bundled` files travel inside the
//! archive under `bundled/`, identified by SHA-256 so the host can verify them without the codec
//! ever touching their bytes.
//!
//! ```json
//! {
//!   "format": "pack-list",
//!   "version": 1,
//!   "files": [{ "path": "mods/a.jar", "sha512": "…", "urls": ["https://…"] }],
//!   "bundled": [{ "path": "config/a.toml", "sha256": "…" }]
//! }
//! ```
//!
//! The codec is written against `msbe-codec-guest` and compiled to `wasm32-unknown-unknown`. It is
//! the reference for third-party formats: a pure translation between the format's records and
//! MSBE's neutral pack records, with no filesystem, network, clock or randomness.

use std::collections::{BTreeMap, BTreeSet};

use msbe_codec_guest::{Codec, Error, Input, Value, json, serde_json};
use serde::{Deserialize, Serialize};

const CODEC_ID: &str = "pack-list";
const MANIFEST: &str = "pack-list.json";
const BUNDLED: &str = "bundled/";
const MANIFEST_LIMIT: u64 = 1 << 20;
/// The host replaces an import origin's digest with the digest of the whole input.
const UNKNOWN_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pack_version: Option<String>,
    #[serde(default)]
    files: Vec<Listed>,
    #[serde(default)]
    bundled: Vec<Bundled>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Listed {
    path: String,
    sha512: String,
    urls: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundled {
    path: String,
    sha256: String,
}

/// The pack-list codec.
#[derive(Debug)]
pub struct PackList;

impl Codec for PackList {
    fn descriptor() -> Value {
        json!({
            "id": CODEC_ID,
            "provider": null,
            "name": "Pack list",
            "extensions": ["packlist"],
            "media_types": ["application/vnd.msbe.pack-list+zip"],
            "directions": { "import": true, "export": true },
            "supported_games": { "kind": "universal" },
            "option_schema": { "schema": 1 }
        })
    }

    fn probe(input: &Input) -> Result<Value, Error> {
        if !input.contains(MANIFEST)? {
            return Ok(json!({ "confidence": 0, "reason": null }));
        }
        let confidence = match read_manifest(input) {
            Ok(manifest) if is_supported(&manifest) => 100,
            Ok(_) | Err(Error::Codec { .. }) => 0,
            Err(error) => return Err(error),
        };
        Ok(json!({ "confidence": confidence, "reason": "pack-list manifest" }))
    }

    fn plan_import(input: &Input, _: Value) -> Result<Value, Error> {
        let manifest = read_manifest(input)?;
        if !is_supported(&manifest) {
            return Err(Error::FormatMismatch);
        }
        let present: BTreeSet<String> = input.entries()?.into_iter().map(|entry| entry.path).collect();
        let requirements: Vec<Value> = manifest
            .files
            .iter()
            .map(|file| {
                json!({
                    "digest": null,
                    "hashes": { "sha512": file.sha512 },
                    "destination": file.path,
                    "side": "required",
                    "sources": [{ "kind": "direct", "urls": file.urls }]
                })
            })
            .collect();
        let mut embedded = Vec::new();
        for file in &manifest.bundled {
            let entry = format!("{BUNDLED}{}", file.path);
            if !present.contains(&entry) {
                return Err(Error::codec(format!("{entry} is listed but not in the archive")));
            }
            embedded.push(json!({
                "entry": entry,
                "digest": format!("sha256:{}", file.sha256),
                "destination": file.path
            }));
        }
        Ok(json!({
            "codec": CODEC_ID,
            "title": manifest.name,
            "origin": {
                "codec": CODEC_ID,
                "pack": manifest.name,
                "version": manifest.pack_version,
                "digest": UNKNOWN_DIGEST
            },
            "target": { "game": null, "game_version": null, "loader": null, "loader_version": null },
            "requirements": requirements,
            "embedded": embedded,
            "warnings": []
        }))
    }

    fn plan_export(request: Value) -> Result<Value, Error> {
        let context = &request["context"];
        let inclusion = &context["inclusion"];
        if inclusion["environment"].as_array().is_some_and(|inputs| !inputs.is_empty()) {
            return Err(Error::Unreproducible {
                message: "pack-list archives cannot declare installation-owned inputs".to_owned(),
            });
        }
        let mut files = Vec::new();
        let mut requirements = Vec::new();
        for requirement in inclusion["requirements"].as_array().into_iter().flatten() {
            files.push(listed(requirement)?);
            requirements.push(requirement.clone());
        }
        let embed: BTreeSet<&str> = inclusion["embed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut seen = BTreeSet::new();
        let mut embedded = Vec::new();
        let mut bundled = Vec::new();
        for file in context["files"].as_array().into_iter().flatten() {
            let (Some(path), Some(digest)) = (file["path"].as_str(), file["digest"].as_str()) else {
                return Err(Error::codec("an export file has no path or digest"));
            };
            if file["deployed"] != Value::Bool(true) || !embed.contains(digest) || !seen.insert(digest) {
                continue;
            }
            let sha256 = digest
                .strip_prefix("sha256:")
                .ok_or_else(|| Error::codec(format!("{path} is not a SHA-256 blob")))?;
            bundled.push(Bundled { path: path.to_owned(), sha256: sha256.to_owned() });
            embedded.push(file.clone());
        }
        let manifest = Manifest {
            format: CODEC_ID.to_owned(),
            version: 1,
            name: None,
            pack_version: None,
            files,
            bundled,
        };
        Ok(json!({
            "codec": CODEC_ID,
            "options": request["options"],
            "embedded": embedded,
            "requirements": requirements,
            "warnings": [],
            "codec_state": serde_json::to_value(&manifest).map_err(Error::codec)?
        }))
    }

    fn layout(plan: Value) -> Result<Value, Error> {
        if plan["codec"] != CODEC_ID {
            return Err(Error::codec("the export plan belongs to another codec"));
        }
        let manifest: Manifest =
            serde_json::from_value(plan["codec_state"].clone()).map_err(Error::codec)?;
        let mut entries = BTreeMap::new();
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(Error::codec)?;
        entries.insert(MANIFEST.to_owned(), json!({ "kind": "inline", "value": bytes }));
        for file in &manifest.bundled {
            entries.insert(
                format!("{BUNDLED}{}", file.path),
                json!({ "kind": "blob", "value": format!("sha256:{}", file.sha256) }),
            );
        }
        let entries: Vec<Value> = entries
            .into_iter()
            .map(|(path, content)| json!({ "path": path, "content": content }))
            .collect();
        Ok(json!({ "container": "zip", "entries": entries }))
    }
}

fn read_manifest(input: &Input) -> Result<Manifest, Error> {
    let bytes = input.read(MANIFEST, MANIFEST_LIMIT)?;
    serde_json::from_slice(&bytes).map_err(Error::codec)
}

fn is_supported(manifest: &Manifest) -> bool {
    manifest.format == CODEC_ID && manifest.version == 1
}

/// A referenced requirement as a listed download: a deployment path, a SHA-512, and HTTPS URLs.
fn listed(requirement: &Value) -> Result<Listed, Error> {
    let path = requirement["destination"].as_str().ok_or_else(|| Error::Unreproducible {
        message: "a referenced file has no deployment path".to_owned(),
    })?;
    let sha512 = requirement["hashes"]["sha512"].as_str().ok_or_else(|| Error::Unreproducible {
        message: format!("{path} has no SHA-512 a pack list can pin"),
    })?;
    let urls: Vec<String> = requirement["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|source| source["kind"] == "direct")
        .flat_map(|source| source["urls"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .map(str::to_owned)
        .collect();
    if urls.is_empty() {
        return Err(Error::Unreproducible {
            message: format!("{path} has no HTTPS download a pack list can reference"),
        });
    }
    Ok(Listed { path: path.to_owned(), sha512: sha512.to_owned(), urls })
}

msbe_codec_guest::export_codec!(PackList);

#[cfg(test)]
mod tests {
    use msbe_codec_guest::{Codec, Input, json};

    use super::PackList;

    #[test]
    fn probe_and_import_read_the_manifest() {
        let manifest = json!({
            "format": "pack-list",
            "version": 1,
            "files": [{ "path": "mods/a.jar", "sha512": "ab", "urls": ["https://example.test/a.jar"] }],
            "bundled": [{ "path": "config/a.toml", "sha256": "cd" }]
        });
        let input = Input::memory([
            ("pack-list.json".to_owned(), manifest.to_string().into_bytes()),
            ("bundled/config/a.toml".to_owned(), b"a = 1".to_vec()),
        ]);
        assert_eq!(PackList::probe(&input).unwrap()["confidence"], 100);
        let plan = PackList::plan_import(&input, json!({})).unwrap();
        assert_eq!(plan["requirements"][0]["destination"], "mods/a.jar");
        assert_eq!(plan["embedded"][0]["entry"], "bundled/config/a.toml");
        assert_eq!(PackList::probe(&Input::memory([])).unwrap()["confidence"], 0);
    }
}
