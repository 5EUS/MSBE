//! An in-memory Modrinth for this crate's tests.

use std::{cell::RefCell, collections::BTreeMap, io::Write};

use msbe_plan_schema::Side;
use msbe_provider_api::{HttpClient, HttpError, Target, hex};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha512};

/// The production API base, which the fake answers for.
pub(crate) const BASE: &str = "https://api.modrinth.com/v2";

/// Canned JSON by URL, hosted files, and a log of every request.
#[derive(Default)]
pub(crate) struct FakeHttp {
    pub(crate) json: BTreeMap<String, Value>,
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    pub(crate) requests: RefCell<Vec<String>>,
}

impl FakeHttp {
    pub(crate) fn route(&mut self, path: &str, body: Value) {
        self.json.insert(format!("{BASE}{path}"), body);
    }

    /// Answers a POST to `path` that asks for `version_types` (comma-separated, or empty when
    /// the request names none).
    pub(crate) fn route_post(&mut self, path: &str, version_types: &str, body: Value) {
        self.json
            .insert(format!("{BASE}{path}#{version_types}"), body);
    }

    fn answer(&self, key: &str, url: &str) -> Result<Vec<u8>, HttpError> {
        self.json
            .get(key)
            .map(|body| serde_json::to_vec(body).unwrap())
            .ok_or_else(|| HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
    }
}

impl HttpClient for FakeHttp {
    fn get(&self, url: &str, query: &[(&str, &str)], _limit: u64) -> Result<Vec<u8>, HttpError> {
        let rendered: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.requests
            .borrow_mut()
            .push(format!("{url}?{}", rendered.join("&")));
        self.answer(url, url)
    }

    fn post_json(&self, url: &str, body: &[u8], _limit: u64) -> Result<Vec<u8>, HttpError> {
        let request: Value = serde_json::from_slice(body).unwrap();
        let version_types = request
            .get("version_types")
            .and_then(Value::as_array)
            .map(|types| {
                types
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        self.requests
            .borrow_mut()
            .push(format!("POST {url} {request}"));
        self.answer(&format!("{url}#{version_types}"), url)
    }

    fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64, HttpError> {
        let bytes = self.files.get(url).ok_or_else(|| HttpError::Status {
            url: url.to_owned(),
            status: 404,
        })?;
        let size = u64::try_from(bytes.len()).unwrap();
        if size > limit {
            return Err(HttpError::TooLarge {
                url: url.to_owned(),
                limit,
            });
        }
        sink.write_all(bytes).unwrap();
        Ok(size)
    }
}

pub(crate) fn contents(file: &str) -> Vec<u8> {
    format!("contents of {file}").into_bytes()
}

pub(crate) fn target() -> Target {
    Target {
        loader: "fabric".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: "1.21.1".to_owned(),
        side: Side::Client,
    }
}

/// A version as Modrinth returns it, including fields this client ignores.
pub(crate) fn version(
    id: &str,
    project: &str,
    number: &str,
    kind: &str,
    date: &str,
    file: &str,
) -> Value {
    let bytes = contents(file);
    json!({
        "id": id,
        "project_id": project,
        "version_number": number,
        "version_type": kind,
        "date_published": date,
        "loaders": ["fabric"],
        "game_versions": ["1.21.1"],
        "author_id": "ignored",
        "changelog": null,
        "downloads": 10,
        "files": [{
            "hashes": { "sha512": hex(&Sha512::digest(&bytes)), "sha1": "unused" },
            "url": format!("https://cdn.modrinth.test/{file}"),
            "filename": file,
            "primary": true,
            "size": bytes.len(),
            "file_type": null
        }],
        "dependencies": []
    })
}

/// Sodium (a newer beta and an older release) and Iris, which requires Sodium.
pub(crate) fn catalogue() -> FakeHttp {
    let mut http = FakeHttp::default();
    let sodium = json!({ "id": "AANobbMI", "slug": "sodium", "title": "Sodium", "project_type": "mod",
                "client_side": "required", "server_side": "required" });
    http.route("/project/sodium", sodium.clone());
    http.route("/project/AANobbMI", sodium);
    http.route(
        "/project/AANobbMI/version",
        json!([
            version(
                "S1",
                "AANobbMI",
                "0.8.12",
                "release",
                "2026-07-06T00:00:00Z",
                "sodium-0.8.12.jar"
            ),
            version(
                "S2",
                "AANobbMI",
                "0.8.13-beta.1",
                "beta",
                "2026-08-07T00:00:00Z",
                "sodium-beta.jar"
            ),
        ]),
    );
    let mut iris = version(
        "I1",
        "YL57xq9U",
        "1.8.0",
        "release",
        "2026-08-01T00:00:00Z",
        "iris-1.8.0.jar",
    );
    iris.as_object_mut().unwrap().insert(
        "dependencies".to_owned(),
        json!([{ "project_id": "AANobbMI", "version_id": null, "dependency_type": "required" }]),
    );
    http.route(
        "/project/iris",
        json!({ "id": "YL57xq9U", "slug": "iris", "title": "Iris", "project_type": "mod",
                "client_side": "required", "server_side": "required" }),
    );
    http.route("/project/YL57xq9U/version", json!([iris]));
    for file in ["sodium-0.8.12.jar", "sodium-beta.jar", "iris-1.8.0.jar"] {
        http.files
            .insert(format!("https://cdn.modrinth.test/{file}"), contents(file));
    }
    http
}

/// The catalogue, plus Fabric API, Quilted Fabric API, which provides it, and a newer Sodium
/// that requires it. Forgified Fabric API, the other stand-in for Fabric API that this crate
/// ships an overlay entry for, is absent, as if Modrinth had removed it.
pub(crate) fn fabric_api_catalogue() -> FakeHttp {
    let mut http = catalogue();
    for (id, slug) in [("P7dR8mSH", "fabric-api"), ("qvIfYCYJ", "qsl")] {
        http.route(
            &format!("/project/{id}"),
            json!({ "id": id, "slug": slug, "title": slug, "project_type": "mod",
                    "client_side": "required", "server_side": "required" }),
        );
        http.route(
            &format!("/project/{id}/version"),
            json!([version(
                &format!("{slug}-1"),
                id,
                "1.0.0",
                "release",
                "2026-08-01T00:00:00Z",
                &format!("{slug}.jar"),
            )]),
        );
    }
    let mut sodium = version(
        "S3",
        "AANobbMI",
        "0.9.0",
        "release",
        "2026-09-01T00:00:00Z",
        "sodium-0.9.0.jar",
    );
    sodium.as_object_mut().unwrap().insert(
        "dependencies".to_owned(),
        json!([{ "project_id": "P7dR8mSH", "version_id": null, "dependency_type": "required" }]),
    );
    http.json
        .get_mut(&format!("{BASE}/project/AANobbMI/version"))
        .and_then(Value::as_array_mut)
        .unwrap()
        .push(sodium);
    http
}
