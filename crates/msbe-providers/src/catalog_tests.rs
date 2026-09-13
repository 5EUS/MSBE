//! Modrinth's shipped provider program, run through the reviewed catalog runtime against canned
//! Modrinth responses.
//!
//! These are the tests the native Modrinth adapter passed before the program replaced it: the
//! program searches, resolves, verifies downloads and finds updates as that adapter did, sending
//! the same requests.

use std::{cell::RefCell, collections::BTreeMap, io::Write};

use msbe_plan_schema::Side;
use msbe_provider_api::{
    AcquisitionError, Adapter, AdapterError, HttpClient, HttpError, Overlay, PackageId, Provenance,
    Target, UpdateCheck, hex,
    model::{Channel, Request},
    resolve::{
        InstallPlan, InstalledRelease, Only, ProjectRequest, Requirement, ResolveError, Resolver,
        Substitution,
    },
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha512};

use crate::Providers;

const ID: &str = "modrinth";
/// The production API base, which the fake answers for.
const BASE: &str = "https://api.modrinth.com/v2";

/// Canned JSON by URL, hosted files, and a log of every request.
#[derive(Default)]
struct FakeHttp {
    json: BTreeMap<String, Value>,
    files: BTreeMap<String, Vec<u8>>,
    requests: RefCell<Vec<String>>,
}

impl FakeHttp {
    fn route(&mut self, path: &str, body: Value) {
        self.json.insert(format!("{BASE}{path}"), body);
    }

    /// Answers a POST to `path` that asks for `version_types` (comma-separated, or empty when the
    /// request names none).
    fn route_post(&mut self, path: &str, version_types: &str, body: Value) {
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

fn contents(file: &str) -> Vec<u8> {
    format!("contents of {file}").into_bytes()
}

fn target() -> Target {
    Target {
        loader: "fabric".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: "1.21.1".to_owned(),
        side: Side::Client,
    }
}

/// A version as Modrinth returns it, including fields the program ignores.
fn version(id: &str, project: &str, number: &str, kind: &str, date: &str, file: &str) -> Value {
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
fn catalogue() -> FakeHttp {
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

/// The catalogue, plus Fabric API, Quilted Fabric API, which provides it, and a newer Sodium that
/// requires it. Forgified Fabric API, the other stand-in the overlay names, is absent, as if
/// Modrinth had removed it.
fn fabric_api_catalogue() -> FakeHttp {
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

fn providers() -> Providers {
    Providers::builtins().unwrap()
}

fn package(project: &str) -> PackageId {
    PackageId {
        provider: ID.to_owned(),
        project: project.to_owned(),
    }
}

fn request(modrinth: &dyn Adapter, raw: &str) -> ProjectRequest {
    match modrinth.request(raw) {
        Ok(Request::Project { reference, version }) => ProjectRequest {
            provider: ID.to_owned(),
            reference,
            version,
        },
        other => panic!("expected a project request for {raw}, got {other:?}"),
    }
}

/// Resolves `requests` against the Fabric client target.
fn plan(
    http: &FakeHttp,
    overlay: &Overlay,
    requests: &[&str],
    with_dependencies: bool,
    installed: &[InstalledRelease],
) -> Result<InstallPlan, ResolveError> {
    let providers = providers();
    let modrinth = providers.adapter(ID).unwrap();
    let requests: Vec<ProjectRequest> = requests.iter().map(|raw| request(modrinth, raw)).collect();
    Resolver {
        adapters: &Only(modrinth),
        http,
        target: &target(),
        overlay,
    }
    .plan_install(&requests, with_dependencies, installed)
}

fn labels(plan: &InstallPlan) -> Vec<(&str, Option<&str>)> {
    plan.selections
        .iter()
        .map(|selection| (selection.project.label(), selection.required_by.as_deref()))
        .collect()
}

#[test]
fn releases_are_filtered_by_target_and_listed_newest_first() {
    let http = catalogue();
    let providers = providers();
    let releases = providers
        .adapter(ID)
        .unwrap()
        .as_releases()
        .unwrap()
        .releases(&http, "AANobbMI", &target())
        .unwrap();
    let listed: Vec<(&str, Channel)> = releases
        .iter()
        .map(|release| (release.id.as_str(), release.channel))
        .collect();
    assert_eq!(listed, [("S2", Channel::Beta), ("S1", Channel::Release)]);
    assert!(http.requests.borrow().iter().any(|request| {
        request.contains("/project/AANobbMI/version?")
            && request.contains(r#"loaders=["fabric"]"#)
            && request.contains(r#"game_versions=["1.21.1"]"#)
            && request.contains("include_changelog=false")
    }));
}

#[test]
fn target_prefilter_honours_loader_capabilities_and_loader_version() {
    let mut http = catalogue();
    let providers = providers();
    let modrinth = providers.adapter(ID).unwrap().as_releases().unwrap();
    let mut quilt = target();
    quilt.loader = "quilt".to_owned();
    quilt.provides = vec!["fabric".to_owned()];
    assert_eq!(
        modrinth.releases(&http, "AANobbMI", &quilt).unwrap().len(),
        2
    );

    let versions = http
        .json
        .get_mut(&format!("{BASE}/project/AANobbMI/version"))
        .and_then(Value::as_array_mut);
    if let Some(versions) = versions {
        for version in versions {
            if let Some(version) = version.as_object_mut() {
                version.insert("loader_versions".to_owned(), json!({ "fabric": ["0.16"] }));
            }
        }
    }
    let mut mismatched = target();
    mismatched.loader_version = Some("0.17".to_owned());
    assert!(
        modrinth
            .releases(&http, "AANobbMI", &mismatched)
            .unwrap()
            .is_empty()
    );
    mismatched.loader_version = Some("0.16".to_owned());
    assert_eq!(
        modrinth
            .releases(&http, "AANobbMI", &mismatched)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn downloads_are_verified_before_they_are_trusted() {
    let mut http = catalogue();
    let providers = providers();
    let modrinth = providers.adapter(ID).unwrap();
    let release = modrinth
        .as_releases()
        .unwrap()
        .releases(&http, "AANobbMI", &target())
        .unwrap()
        .into_iter()
        .find(|release| release.id == "S1")
        .unwrap();
    let file = release.primary_file().unwrap().clone();
    let good = tempfile::tempdir().unwrap();
    let acquired = modrinth.acquire(&http, &file, good.path()).unwrap();
    assert_eq!(
        std::fs::read(&acquired.path).unwrap(),
        contents("sodium-0.8.12.jar")
    );
    let provenance = modrinth.provenance(&release, &acquired);
    assert_eq!(
        (provenance.provider.as_str(), provenance.version.as_str()),
        (ID, "S1")
    );
    assert_eq!(provenance.hashes.get("sha512"), file.sha512.as_ref());

    // Same size, one bit different: only the hash can catch it.
    let mut tampered = contents("sodium-0.8.12.jar");
    if let Some(last) = tampered.last_mut() {
        *last ^= 1;
    }
    http.files.insert(file.url.clone(), tampered);
    let bad = tempfile::tempdir().unwrap();
    assert!(matches!(
        modrinth.acquire(&http, &file, bad.path()),
        Err(AdapterError::Acquisition(
            AcquisitionError::HashMismatch { .. }
        ))
    ));

    let mut insecure = file.clone();
    insecure.url = insecure.url.replacen("https://", "http://", 1);
    assert!(matches!(
        modrinth.acquire(&http, &insecure, bad.path()),
        Err(AdapterError::Acquisition(AcquisitionError::InsecureUrl(_)))
    ));

    let mut escaping = file;
    escaping.name = "../escape.jar".to_owned();
    assert!(matches!(
        modrinth.acquire(&http, &escaping, bad.path()),
        Err(AdapterError::Acquisition(AcquisitionError::UnsafeFileName(
            _
        )))
    ));
}

#[test]
fn search_asks_for_mods_compatible_with_the_target() {
    let mut http = FakeHttp::default();
    http.route(
        "/search",
        json!({
            "hits": [
                { "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
                  "description": "A rendering engine", "downloads": 42,
                  "icon_url": "https://cdn.modrinth.com/data/AANobbMI/icon.png",
                  "client_side": "required", "server_side": "required", "author": "ignored" },
                { "project_id": "SERVERONLY", "slug": "server-only", "title": "Server only",
                  "client_side": "unsupported", "server_side": "required" }
            ],
            "offset": 0, "limit": 5, "total_hits": 2
        }),
    );
    let providers = providers();
    let hits = providers
        .adapter(ID)
        .unwrap()
        .as_search()
        .unwrap()
        .search(&http, "render", &target(), 200)
        .unwrap();
    let [hit] = hits.as_slice() else {
        panic!("expected one client-compatible hit, got {hits:?}");
    };
    assert_eq!(
        (hit.provider.as_str(), hit.reference.as_str()),
        (ID, "sodium")
    );
    assert_eq!(
        hit.icon_url.as_deref(),
        Some("https://cdn.modrinth.com/data/AANobbMI/icon.png")
    );

    let requests = http.requests.borrow();
    let request = requests.first().unwrap();
    assert!(request.contains("query=render"), "{request}");
    assert!(
        request
            .contains(r#"facets=[["project_type:mod"],["versions:1.21.1"],["categories:fabric"]]"#),
        "{request}"
    );
    assert!(
        request.contains("limit=100"),
        "the limit is lowered to Modrinth's maximum: {request}"
    );
}

#[test]
fn ships_overlay_entries_for_fabric_api_reimplementations() {
    let overlay = Overlay::from_toml(msbe_provider_modrinth::PROGRAM.overlay).unwrap();
    let suppliers: Vec<&str> = overlay
        .suppliers(&package("P7dR8mSH"))
        .map(|supplier| supplier.project.as_str())
        .collect();
    assert_eq!(suppliers, ["Aqlf1Shp", "qvIfYCYJ"]);
}

#[test]
fn malformed_references_are_refused_before_any_request() {
    let providers = providers();
    let modrinth = providers.adapter(ID).unwrap();
    for raw in ["../etc/passwd", "", "@1.0", "sodium@", "a/b", "sodium@1 0"] {
        let error = modrinth.request(raw).unwrap_err();
        assert!(
            error.to_string().contains("not a valid Modrinth"),
            "{raw}: {error}"
        );
    }
    assert!(matches!(
        modrinth.request("sodium@mc1.21.1-0.8.13+fabric"),
        Ok(Request::Project { version: Some(version), .. }) if version == "mc1.21.1-0.8.13+fabric"
    ));
}

#[test]
fn updates_stay_on_their_channel_and_only_go_back_to_regain_compatibility() {
    let hash = |c: char| c.to_string().repeat(128);
    let dated = |id: &str, kind: &str, date: &str, game_version: &str| {
        let mut entry = version(id, "P", id, kind, date, &format!("{id}.jar"));
        entry
            .as_object_mut()
            .unwrap()
            .insert("game_versions".to_owned(), json!([game_version]));
        entry
    };
    let mut http = FakeHttp::default();
    http.route_post(
        "/version_files",
        "",
        json!({
            hash('a'): dated("R1", "release", "2026-07-01T00:00:00Z", "1.21.1"),
            hash('b'): dated("B1", "beta", "2026-08-10T00:00:00Z", "1.21.1"),
            hash('c'): dated("O1", "release", "2026-09-01T00:00:00Z", "1.20.1"),
            hash('d'): dated("G1", "release", "2026-09-01T00:00:00Z", "1.20.1"),
        }),
    );
    http.route_post(
        "/version_files/update",
        "release",
        json!({
            // Newer on the release channel: an update.
            hash('a'): dated("R2", "release", "2026-08-01T00:00:00Z", "1.21.1"),
            // Older, but the installed version does not support 1.21.1: still offered.
            hash('c'): dated("O2", "release", "2026-01-01T00:00:00Z", "1.21.1"),
        }),
    );
    http.route_post(
        "/version_files/update",
        "release,beta",
        // Older than the installed beta, so not a downgrade target.
        json!({ hash('b'): dated("R3", "release", "2026-08-01T00:00:00Z", "1.21.1") }),
    );

    // The first hash is uppercase: lookups are case-insensitive.
    let installed: Vec<Provenance> = ['A', 'b', 'c', 'd', 'e']
        .into_iter()
        .map(|key| Provenance {
            provider: ID.to_owned(),
            project: "P".to_owned(),
            version: String::new(),
            version_number: String::new(),
            hashes: BTreeMap::from([("sha512".to_owned(), hash(key))]),
        })
        .collect();
    let installed: Vec<&Provenance> = installed.iter().collect();
    let providers = providers();
    let checks = providers
        .adapter(ID)
        .unwrap()
        .as_updates()
        .unwrap()
        .check(&http, &installed, &target())
        .unwrap();
    let available = |index: usize| match checks.get(index) {
        Some(UpdateCheck::Available(update)) => Some(update.release.id.as_str()),
        _ => None,
    };
    assert_eq!(available(0), Some("R2"));
    assert_eq!(checks.get(1), Some(&UpdateCheck::Current));
    assert_eq!(available(2), Some("O2"));
    assert_eq!(checks.get(3), Some(&UpdateCheck::Incompatible));
    assert_eq!(checks.get(4), Some(&UpdateCheck::Unlisted));

    let requests = http.requests.borrow();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(
        requests.iter().any(|request| {
            request.contains(r#""version_types":["release","beta"]"#)
                && request.contains(r#""loaders":["fabric"]"#)
                && request.contains(r#""game_versions":["1.21.1"]"#)
                && request.contains(r#""algorithm":"sha512""#)
        }),
        "{requests:?}"
    );
}

#[test]
fn required_dependencies_are_walked_only_when_asked() {
    let http = catalogue();
    let none = Overlay::default();

    let with = plan(&http, &none, &["iris"], true, &[]).unwrap();
    assert_eq!(labels(&with), [("iris", None), ("sodium", Some("iris"))]);
    assert!(with.unresolved.is_empty());

    let without = plan(&http, &none, &["iris"], false, &[]).unwrap();
    assert_eq!(labels(&without), [("iris", None)]);
    assert_eq!(
        without.unresolved,
        [Requirement {
            provider: ID.to_owned(),
            project_id: "AANobbMI".to_owned(),
            declared_by: "iris".to_owned(),
        }]
    );
}

#[test]
fn pins_name_a_release_by_id_or_version_number() {
    let http = catalogue();
    let none = Overlay::default();
    let chosen = |raw: &str| {
        plan(&http, &none, &[raw], false, &[]).map(|plan| {
            plan.selections
                .first()
                .map(|selection| selection.release.id.clone())
        })
    };
    // Unpinned, the newest compatible version wins, whatever its channel.
    assert_eq!(chosen("sodium").unwrap().as_deref(), Some("S2"));
    assert_eq!(chosen("sodium@0.8.12").unwrap().as_deref(), Some("S1"));
    assert_eq!(chosen("sodium@S1").unwrap().as_deref(), Some("S1"));
    assert!(matches!(
        chosen("sodium@9.9.9"),
        Err(ResolveError::Solver(_))
    ));
}

#[test]
fn dependency_solver_backtracks_to_satisfy_an_exact_transitive_release() {
    let mut http = catalogue();
    let versions = |http: &mut FakeHttp, project: &str| {
        http.json
            .get_mut(&format!("{BASE}/project/{project}/version"))
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .unwrap()
    };
    let mut sodium = versions(&mut http, "AANobbMI");
    sodium.push(version(
        "S3",
        "AANobbMI",
        "0.9.0",
        "release",
        "2026-09-01T00:00:00Z",
        "sodium-0.9.0.jar",
    ));
    http.route("/project/AANobbMI/version", Value::Array(sodium));
    let mut iris = versions(&mut http, "YL57xq9U");
    if let Some(iris) = iris.first_mut().and_then(Value::as_object_mut) {
        iris.insert(
            "dependencies".to_owned(),
            json!([{ "project_id": "AANobbMI", "version_id": "S1", "dependency_type": "required" }]),
        );
    }
    http.route("/project/YL57xq9U/version", Value::Array(iris));

    let resolved = plan(&http, &Overlay::default(), &["iris", "sodium"], true, &[]).unwrap();
    let sodium = resolved
        .selections
        .iter()
        .find(|selection| selection.project.id == package("AANobbMI"))
        .unwrap();
    assert_eq!(sodium.release.id, "S1");
}

#[test]
fn a_project_reached_twice_is_selected_once() {
    let http = catalogue();
    let resolved = plan(&http, &Overlay::default(), &["sodium", "iris"], true, &[]).unwrap();
    let names: Vec<&str> = resolved
        .selections
        .iter()
        .map(|selection| selection.project.label())
        .collect();
    assert_eq!(names, ["sodium", "iris"]);
}

#[test]
fn a_project_that_does_not_support_the_target_side_is_refused() {
    let mut http = catalogue();
    if let Some(project) = http
        .json
        .get_mut(&format!("{BASE}/project/sodium"))
        .and_then(Value::as_object_mut)
    {
        project.insert("server_side".to_owned(), json!("unsupported"));
    }
    let providers = providers();
    let modrinth = providers.adapter(ID).unwrap();
    let mut server = target();
    server.side = Side::Server;
    let resolved = Resolver {
        adapters: &Only(modrinth),
        http: &http,
        target: &server,
        overlay: &Overlay::default(),
    }
    .plan_install(&[request(modrinth, "sodium")], false, &[]);
    assert!(
        matches!(
            &resolved,
            Err(ResolveError::UnsupportedSide {
                side: Side::Server,
                ..
            })
        ),
        "{resolved:?}"
    );
}

#[test]
fn an_installed_stand_in_meets_requirements_and_excludes_the_original() {
    let http = fabric_api_catalogue();
    let overlay = Overlay::from_toml(msbe_provider_modrinth::PROGRAM.overlay).unwrap();

    // Nothing stands in yet, so Fabric API is preferred over the fork that provides it, and the
    // stand-in Modrinth no longer has does not fail resolution.
    let fresh = plan(&http, &overlay, &["sodium"], true, &[]).unwrap();
    assert_eq!(
        labels(&fresh),
        [("sodium", None), ("fabric-api", Some("sodium"))]
    );
    assert!(fresh.substitutions.is_empty());

    let quilted = [InstalledRelease {
        package: package("qvIfYCYJ"),
        release: "qsl-1".to_owned(),
    }];
    let substitution = Substitution {
        requirement: Requirement {
            provider: ID.to_owned(),
            project_id: "P7dR8mSH".to_owned(),
            declared_by: "sodium".to_owned(),
        },
        supplied_by: package("qvIfYCYJ"),
    };
    let kept = plan(&http, &overlay, &["sodium"], true, &quilted).unwrap();
    assert_eq!(labels(&kept), [("sodium", None), ("qsl", Some("sodium"))]);
    assert_eq!(kept.substitutions, std::slice::from_ref(&substitution));

    // Without walking dependencies, the installed fork is never fetched, yet it still meets the
    // requirement instead of leaving it unresolved.
    let bare = plan(&http, &overlay, &["sodium"], false, &quilted).unwrap();
    assert_eq!(labels(&bare), [("sodium", None)]);
    assert!(bare.unresolved.is_empty(), "{:?}", bare.unresolved);
    assert_eq!(bare.substitutions, [substitution]);

    assert!(matches!(
        plan(&http, &overlay, &["P7dR8mSH"], false, &quilted),
        Err(ResolveError::Solver(_))
    ));
}

#[test]
fn relationships_resolve_dependencies_that_name_only_a_version() {
    fn ids(list: &[Requirement]) -> Vec<&str> {
        list.iter()
            .map(|requirement| requirement.project_id.as_str())
            .collect()
    }

    let mut http = FakeHttp::default();
    http.route(
        "/version/V9",
        version(
            "V9",
            "QQQ",
            "2.0",
            "release",
            "2026-01-01T00:00:00Z",
            "q.jar",
        ),
    );
    let mut iris = version(
        "I1",
        "YL57xq9U",
        "1.8.0",
        "release",
        "2026-08-01T00:00:00Z",
        "iris.jar",
    );
    iris.as_object_mut().unwrap().insert(
        "dependencies".to_owned(),
        json!([
            { "project_id": "AANobbMI", "dependency_type": "required" },
            { "version_id": "V9", "dependency_type": "required" },
            { "project_id": "XXX", "dependency_type": "incompatible" },
            { "project_id": "YYY", "dependency_type": "optional" }
        ]),
    );
    http.route("/project/YL57xq9U/version", json!([iris]));

    let providers = providers();
    let modrinth = providers.adapter(ID).unwrap();
    let release = modrinth
        .as_releases()
        .unwrap()
        .releases(&http, "YL57xq9U", &target())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let found = Resolver {
        adapters: &Only(modrinth),
        http: &http,
        target: &target(),
        overlay: &Overlay::default(),
    }
    .relationships(&release, "iris")
    .unwrap();
    assert_eq!(ids(&found.required), ["AANobbMI", "QQQ"]);
    assert_eq!(ids(&found.incompatible), ["XXX"]);
    assert!(found.required.iter().all(|r| r.declared_by == "iris"));
}
