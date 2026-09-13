//! In-memory Modrinth service for integration tests of consumers of this adapter.

#![expect(
    clippy::type_complexity,
    reason = "the opt-in test fixture intentionally uses infallible test setup and shared mutable canned responses"
)]

use std::{cell::RefCell, collections::BTreeMap, fmt::Write as _, io::Write, rc::Rc};

use msbe_provider_api::{HttpClient, HttpError, HttpRequest, HttpResponse, Method};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha512};

const API: &str = "https://api.modrinth.com/v2";
const CDN: &str = "https://cdn.modrinth.test";

/// Shared mutable routes and hosted contents for an in-memory Modrinth service.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeModrinth {
    /// JSON responses by absolute API URL.
    pub(crate) json: Rc<RefCell<BTreeMap<String, Value>>>,
    /// Download contents by absolute CDN URL.
    pub(crate) files: Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
}

impl FakeModrinth {
    /// A catalogue containing Sodium, Iris, and Iris's required Sodium dependency.
    pub(crate) fn catalogue() -> Self {
        let fake = Self::default();
        fake.publish(
            "sodium",
            "0.8.12",
            "release",
            "2026-07-06T00:00:00Z",
            &json!([]),
        );
        fake.publish(
            "iris",
            "1.8.0",
            "release",
            "2026-07-06T00:00:00Z",
            &requires("AANobbMI"),
        );
        fake.json.borrow_mut().insert(format!("{API}/search"), json!({ "hits": [{ "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium", "description": "A rendering engine", "downloads": 42, "client_side": "required", "server_side": "required" }] }));
        fake
    }

    /// Publishes a project release with a deterministic downloadable JAR.
    pub(crate) fn publish(
        &self,
        slug: &str,
        number: &str,
        kind: &str,
        date: &str,
        dependencies: &Value,
    ) {
        let id = project_id(slug);
        let file = format!("{slug}-fabric-{number}.jar");
        let bytes = format!("{slug} {number} jar bytes").into_bytes();
        let version = json!({ "id": format!("{id}-{number}"), "project_id": id, "version_number": number, "version_type": kind, "date_published": date, "loaders": ["fabric"], "game_versions": ["1.21.1"], "files": [{ "hashes": { "sha512": hex(&Sha512::digest(&bytes)) }, "url": format!("{CDN}/{file}"), "filename": file, "primary": true, "size": bytes.len() }], "dependencies": dependencies });
        let project = json!({ "id": id, "slug": slug, "title": slug, "project_type": "mod", "client_side": "required", "server_side": "required" });
        let mut json = self.json.borrow_mut();
        json.insert(format!("{API}/project/{slug}"), project.clone());
        json.insert(format!("{API}/project/{id}"), project);
        if let Some(versions) = json
            .entry(format!("{API}/project/{id}/version"))
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            versions.push(version);
        }
        drop(json);
        self.files
            .borrow_mut()
            .insert(format!("{CDN}/{file}"), bytes);
    }

    /// Corrupts a hosted file without changing its declared size.
    pub(crate) fn corrupt(&self, file: &str) {
        if let Some(bytes) = self.files.borrow_mut().get_mut(&format!("{CDN}/{file}"))
            && let Some(last) = bytes.last_mut()
        {
            *last ^= 1;
        }
    }

    fn versions(&self) -> Vec<Value> {
        self.json
            .borrow()
            .iter()
            .filter(|(url, _)| url.ends_with("/version"))
            .filter_map(|(_, list)| list.as_array())
            .flatten()
            .cloned()
            .collect()
    }
}

impl HttpClient for FakeModrinth {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let url = request.url;
        let Method::Post { json } = request.method else {
            return self
                .json
                .borrow()
                .get(url)
                .map(|body| serde_json::to_vec(body).unwrap().into())
                .ok_or_else(|| HttpError::Status {
                    url: url.to_owned(),
                    status: 404,
                });
        };
        let request: Value = serde_json::from_slice(json).unwrap();
        let versions = self.versions();
        let latest = url.ends_with("/version_files/update");
        let mut answer = serde_json::Map::new();
        for hash in at(&request, "/hashes").as_array().unwrap() {
            let Some(installed) = versions
                .iter()
                .find(|version| at(version, "/files/0/hashes/sha512") == hash)
            else {
                continue;
            };
            let found = if latest {
                versions
                    .iter()
                    .filter(|version| at(version, "/project_id") == at(installed, "/project_id"))
                    .filter(|version| matches_request(version, &request))
                    .max_by_key(|version| at(version, "/date_published").to_string())
            } else {
                Some(installed)
            };
            if let Some(version) = found {
                answer.insert(hash.as_str().unwrap().to_owned(), version.clone());
            }
        }
        Ok(serde_json::to_vec(&answer).unwrap().into())
    }
    fn download(&self, request: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError> {
        let url = request.url;
        let files = self.files.borrow();
        let bytes = files.get(url).ok_or_else(|| HttpError::Status {
            url: url.to_owned(),
            status: 404,
        })?;
        sink.write_all(bytes).unwrap();
        Ok(u64::try_from(bytes.len()).unwrap())
    }
}

fn project_id(slug: &str) -> &'static str {
    match slug {
        "sodium" => "AANobbMI",
        "iris" => "YL57xq9U",
        "fabric-api" => "P7dR8mSH",
        "qsl" => "qvIfYCYJ",
        _ => panic!("no id for test project {slug}"),
    }
}
fn requires(project: &str) -> Value {
    json!([{ "project_id": project, "version_id": null, "dependency_type": "required" }])
}
fn matches_request(version: &Value, request: &Value) -> bool {
    let wanted_type = request
        .get("version_types")
        .and_then(Value::as_array)
        .is_none_or(|types| types.contains(at(version, "/version_type")));
    let supported = at(version, "/game_versions")
        .as_array()
        .unwrap()
        .contains(at(request, "/game_versions/0"));
    wanted_type && supported
}
fn at<'a>(document: &'a Value, pointer: &str) -> &'a Value {
    document
        .pointer(pointer)
        .unwrap_or_else(|| panic!("no {pointer} in {document}"))
}
fn hex(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").unwrap();
        out
    })
}
