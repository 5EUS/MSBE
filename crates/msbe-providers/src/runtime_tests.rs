//! The reviewed catalog runtime against a catalog shaped unlike Modrinth: one serving several games
//! by number, identifying loaders by number in requests, publishing SHA-1 and MD5 digests in keyed
//! arrays, flagging files their authors keep off third-party tools, and offering no hash lookup.

use std::{cell::RefCell, collections::BTreeMap, io::Write};

use md5::Md5;
use msbe_plan_schema::Side;
use msbe_provider_api::{
    AcquisitionError, Adapter, AdapterError, HttpClient, HttpError, HttpRequest, HttpResponse,
    Method, Provenance, ProviderProgram, Target, UpdateCheck, hex,
    model::{ActionReason, Channel, DependencyKind, Download, Release},
};
use serde_json::{Value, json};
use sha1::{Digest as _, Sha1};

use crate::runtime;

const API: &str = "https://api.catalog.test";

const PROGRAM: &str = r#"
runtime = "catalog-v1"
capabilities = ["search", "project", "releases", "updates"]

[games]
game = "432"
saga = { id = "1704", editions = { original = "73" } }

[translate]
loader = { modern = "Modern", legacy = "Legacy" }
storefront = { sandboxed = "Sandboxed" }

[provider]
schema = 1
id = "catalog"
name = "Catalog"
[provider.source]
type = "prefixed"
prefix = "catalog:"
[provider.metadata]
api_base = "https://api.catalog.test"
[provider.acquisition]
type = "direct_https"
[provider.policy]
requires_auth = false
respects_distribution_flag = true
tos_url = ""
ack_required = false

[routes]
search = "/v1/games/{game}/mods/search"
project = "/v1/mods/{reference}"
releases = "/v1/mods/{project}/files"

[pages]
release = "https://www.catalog.test/{game}/mods/{project}/files/{release}"

[search]
query = "searchFilter"
limit = "pageSize"
facets = { parameter = "facets", groups = [["kind:mod"], ["edition:{edition}"]] }
parameters = [
  { name = "gameId", target = "game", encoding = "single" },
  { name = "modLoaderType", target = "loaders", encoding = "single", values = { modern = "4", legacy = "1" } },
  { name = "gameVersion", target = "game-version", encoding = "single" },
]

[releases]
order = "semver"
query = [
  { name = "loaders", target = "loaders", encoding = "comma" },
  { name = "store", target = "storefront", encoding = "repeated" },
]

[updates]
type = "releases-v1"

[mappings]
search_items = "/data"
releases = "/data"

[mappings.hit]
id = "/id"
title = "/name"
games = "/gameId"

[mappings.project]
id = "/data/id"
title = "/data/name"
games = "/data/gameId"

[mappings.release]
id = "/id"
project = "/modId"
number = "/displayName"
published = "/fileDate"
files = { single = "" }
game_versions = "/gameVersions"
loaders = "/gameVersions"
editions = "/editions"
storefronts = { each = "/stores", value = "/name" }
dependencies = "/dependencies"
channel = { pointer = "/releaseType", release = "1", beta = "2", alpha = "3" }

[mappings.release.file]
url = "/downloadUrl"
name = "/fileName"
size = "/fileLength"
sha1 = { each = "/hashes", value = "/value", when = { pointer = "/algo", equals = "1" } }
md5 = { each = "/hashes", value = "/value", when = { pointer = "/algo", equals = "2" } }
distributable = "/isAvailable"
extension = "zip"

[mappings.release.dependency]
project = "/modId"
kind = "/relationType"
kinds = { required = "3", optional = "2", incompatible = "5", embedded = "1" }
"#;

/// Canned JSON by URL, hosted files, and a log of every request.
#[derive(Default)]
struct FakeHttp {
    json: BTreeMap<String, Value>,
    files: BTreeMap<String, Vec<u8>>,
    requests: RefCell<Vec<String>>,
}

impl FakeHttp {
    fn route(&mut self, path: &str, body: Value) {
        self.json.insert(format!("{API}{path}"), body);
    }

    fn requested(&self) -> Vec<String> {
        self.requests.borrow().clone()
    }
}

impl HttpClient for FakeHttp {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let url = request.url;
        assert_eq!(
            request.method,
            Method::Get,
            "this catalog is never posted to: {url}"
        );
        let rendered: Vec<String> = request
            .query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        self.requests
            .borrow_mut()
            .push(format!("{url}?{}", rendered.join("&")));
        self.json
            .get(url)
            .map(|body| serde_json::to_vec(body).unwrap().into())
            .ok_or_else(|| HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
    }

    fn download(&self, request: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError> {
        let url = request.url;
        self.requests.borrow_mut().push(format!("DOWNLOAD {url}"));
        let bytes = self.files.get(url).ok_or_else(|| HttpError::Status {
            url: url.to_owned(),
            status: 404,
        })?;
        sink.write_all(bytes).unwrap();
        Ok(u64::try_from(bytes.len()).unwrap())
    }
}

fn program_adapter(document: &str) -> Box<dyn Adapter> {
    let program: ProviderProgram = toml::from_str(document).unwrap();
    program.validate().unwrap();
    runtime::build(program)
}

fn target() -> Target {
    Target {
        game: "game".to_owned(),
        edition: None,
        storefront: None,
        loader: "modern".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: Some("1.0".to_owned()),
        side: Side::Client,
    }
}

fn bytes(id: u64) -> Vec<u8> {
    format!("file {id}").into_bytes()
}

/// A file as the catalog lists it, in the release channel, supporting game version 1.0 and the
/// modern loader.
fn file(id: u64, number: &str, date: &str) -> Value {
    let contents = bytes(id);
    json!({
        "id": id,
        "modId": 100,
        "displayName": number,
        "fileDate": date,
        "fileName": format!("mod-{number}"),
        "fileLength": contents.len(),
        "downloadUrl": format!("https://cdn.catalog.test/{id}"),
        "releaseType": 1,
        "gameVersions": ["1.0", "Modern"],
        "hashes": [
            { "algo": 2, "value": hex(&Md5::digest(&contents)) },
            { "algo": 1, "value": hex(&Sha1::digest(&contents)) },
        ],
        "dependencies": [{ "modId": 77, "relationType": 3 }, { "modId": 78, "relationType": 6 }],
    })
}

fn with(mut value: Value, key: &str, field: Value) -> Value {
    value.as_object_mut().unwrap().insert(key.to_owned(), field);
    value
}

/// Mod 100's files: 1.2.0, a 1.3.0 beta, 1.10.0, and a 2.0.0 for another game version.
fn catalogue() -> FakeHttp {
    let mut http = FakeHttp::default();
    http.route(
        "/v1/mods/100",
        json!({ "data": { "id": 100, "name": "Mod", "gameId": 432 } }),
    );
    http.route(
        "/v1/mods/200",
        json!({ "data": { "id": 200, "name": "Elsewhere", "gameId": 73 } }),
    );
    http.route(
        "/v1/mods/100/files",
        json!({ "data": [
            file(501, "1.2.0", "2026-01-01T00:00:00Z"),
            with(file(502, "1.3.0-beta.1", "2026-03-01T00:00:00Z"), "releaseType", json!(2)),
            file(503, "1.10.0", "2026-02-01T00:00:00Z"),
            with(file(504, "2.0.0", "2026-04-01T00:00:00Z"), "gameVersions", json!(["2.0", "Modern"])),
        ] }),
    );
    for id in 501..=504 {
        http.files
            .insert(format!("https://cdn.catalog.test/{id}"), bytes(id));
    }
    http
}

fn releases(adapter: &dyn Adapter, http: &FakeHttp, target: &Target) -> Vec<Release> {
    adapter
        .as_releases()
        .unwrap()
        .releases(http, "100", target)
        .unwrap()
}

fn provenance(version: &str, number: &str) -> Provenance {
    Provenance {
        provider: "catalog".to_owned(),
        project: "100".to_owned(),
        version: version.to_owned(),
        version_number: number.to_owned(),
        hashes: BTreeMap::new(),
    }
}

#[test]
fn a_catalog_serving_several_games_addresses_each_by_its_own_identifier() {
    let adapter = program_adapter(PROGRAM);
    let search = adapter.as_search().unwrap();
    let http = FakeHttp::default();

    let mut other = target();
    other.game = "other".to_owned();
    let refused = search.search(&http, "words", &other, 10).unwrap_err();
    assert!(
        refused.to_string().contains("does not serve other"),
        "{refused}"
    );
    assert!(http.requested().is_empty());

    let mut saga = target();
    saga.game = "saga".to_owned();
    for (edition, id) in [
        (Some("original"), "73"),
        (Some("remaster"), "1704"),
        (None, "1704"),
    ] {
        saga.edition = edition.map(str::to_owned);
        drop(search.search(&http, "words", &saga, 10));
        let last = http.requested().pop().unwrap();
        assert!(
            last.starts_with(&format!("{API}/v1/games/{id}/mods/search?"))
                && last.contains(&format!("gameId={id}")),
            "{last}"
        );
    }
}

#[test]
fn facts_are_translated_and_encoded_as_each_parameter_declares() {
    let adapter = program_adapter(PROGRAM);
    let mut http = catalogue();
    http.route(
        "/v1/games/432/mods/search",
        json!({ "data": [{ "id": 100, "name": "Mod", "gameId": 432 }, { "id": 200, "name": "Elsewhere", "gameId": 73 }] }),
    );
    let search = adapter.as_search().unwrap();

    let hits = search.search(&http, "words", &target(), 10).unwrap();
    assert_eq!(
        hits.iter()
            .map(|hit| hit.project.as_str())
            .collect::<Vec<_>>(),
        ["100"]
    );
    let request = http.requested().pop().unwrap();
    for expected in [
        "searchFilter=words",
        r#"facets=[["kind:mod"]]"#,
        "gameId=432",
        "modLoaderType=4",
        "gameVersion=1.0",
        "pageSize=10",
    ] {
        assert!(request.contains(expected), "{expected} in {request}");
    }

    let mut unversioned = target();
    unversioned.game_version = None;
    unversioned.loader = "legacy".to_owned();
    unversioned.provides = vec!["modern".to_owned()];
    unversioned.edition = Some("original".to_owned());
    search.search(&http, "words", &unversioned, 10).unwrap();
    let request = http.requested().pop().unwrap();
    assert!(
        request.contains(r#"facets=[["kind:mod"],["edition:original"]]"#),
        "{request}"
    );
    assert!(!request.contains("gameVersion"), "{request}");
    assert!(
        !request.contains("modLoaderType"),
        "two loaders cannot be one value: {request}"
    );

    releases(adapter.as_ref(), &http, &unversioned);
    let request = http.requested().pop().unwrap();
    assert!(
        request.contains("loaders=Legacy,Modern") && !request.contains("store="),
        "{request}"
    );
    let mut storefront = target();
    storefront.storefront = Some("sandboxed".to_owned());
    storefront.loader = "unmapped".to_owned();
    releases(adapter.as_ref(), &http, &storefront);
    let request = http.requested().pop().unwrap();
    assert!(
        request.contains("store=Sandboxed") && !request.contains("loaders="),
        "{request}"
    );
}

#[test]
fn numeric_records_are_read_filtered_and_ordered_by_semantic_version() {
    let adapter = program_adapter(PROGRAM);
    let mut http = catalogue();
    let listed = releases(adapter.as_ref(), &http, &target());
    let summary: Vec<(&str, &str, Channel)> = listed
        .iter()
        .map(|release| {
            (
                release.id.as_str(),
                release.number.as_str(),
                release.channel,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("503", "1.10.0", Channel::Release),
            ("502", "1.3.0-beta.1", Channel::Beta),
            ("501", "1.2.0", Channel::Release),
        ]
    );
    let newest = listed.first().unwrap();
    let file = newest.primary_file().unwrap();
    assert_eq!(file.name, "mod-1.10.0.zip");
    assert_eq!(
        file.sha1.as_deref(),
        Some(hex(&Sha1::digest(bytes(503))).as_str())
    );
    assert_eq!(
        file.md5.as_deref(),
        Some(hex(&Md5::digest(bytes(503))).as_str())
    );
    let kinds: Vec<(Option<&str>, DependencyKind)> = newest
        .dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.project.as_ref().map(|id| id.project.as_str()),
                dependency.kind,
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (Some("77"), DependencyKind::Required),
            (Some("78"), DependencyKind::Unknown)
        ]
    );

    let mut legacy = target();
    legacy.loader = "legacy".to_owned();
    assert!(releases(adapter.as_ref(), &http, &legacy).is_empty());

    let files = http
        .json
        .get_mut(&format!("{API}/v1/mods/100/files"))
        .unwrap();
    let listing = files.pointer_mut("/data").unwrap().as_array_mut().unwrap();
    for (index, value) in listing.iter_mut().enumerate() {
        let object = value.as_object_mut().unwrap();
        if index == 0 {
            object.insert("stores".to_owned(), json!([{ "name": "Sandboxed" }]));
            object.insert("editions".to_owned(), json!(["original"]));
        } else {
            object.insert("stores".to_owned(), json!([{ "name": "Elsewhere" }]));
        }
    }
    let mut sandboxed = target();
    sandboxed.storefront = Some("sandboxed".to_owned());
    let ids = |target: &Target| -> Vec<String> {
        releases(adapter.as_ref(), &http, target)
            .into_iter()
            .map(|release| release.id)
            .collect()
    };
    assert_eq!(ids(&sandboxed), ["501"]);
    sandboxed.edition = Some("remaster".to_owned());
    assert!(ids(&sandboxed).is_empty());
    assert_eq!(ids(&target()), ["503", "502", "501"]);
}

#[test]
fn weak_published_digests_are_verified_and_kept_for_hash_lookups() {
    let adapter = program_adapter(PROGRAM);
    let mut http = catalogue();
    let release = releases(adapter.as_ref(), &http, &target())
        .into_iter()
        .find(|release| release.id == "501")
        .unwrap();
    let file = release.primary_file().unwrap().clone();
    let good = tempfile::tempdir().unwrap();
    let acquired = adapter.acquire(&http, &file, good.path()).unwrap();
    assert!(
        !adapter
            .provenance(&release, &acquired)
            .hashes
            .contains_key("sha1")
    );

    let lookup = program_adapter(&PROGRAM.replace(
        "[updates]\ntype = \"releases-v1\"",
        "[updates]\ntype = \"hash-lookup-v1\"\nalgorithm = \"sha1\"\nlisted = \"/v1/fingerprints\"\nlatest = \"/v1/fingerprints/latest\"\nfields = { hashes = \"hashes\", algorithm = \"algorithm\", loaders = \"loaders\", game_versions = \"versions\", channels = \"channels\" }",
    ));
    assert_eq!(
        lookup.provenance(&release, &acquired).hashes.get("sha1"),
        file.sha1.as_ref()
    );

    http.files.insert(
        "https://cdn.catalog.test/501".to_owned(),
        b"file 50X".to_vec(),
    );
    let bad = tempfile::tempdir().unwrap();
    assert!(matches!(
        adapter.acquire(&http, &file, bad.path()),
        Err(AdapterError::Acquisition(AcquisitionError::HashMismatch {
            algorithm: "SHA-1",
            ..
        }))
    ));
}

#[test]
fn files_msbe_may_not_download_send_the_user_to_their_page() {
    let adapter = program_adapter(PROGRAM);
    let mut http = catalogue();
    let listing = http
        .json
        .get_mut(&format!("{API}/v1/mods/100/files"))
        .and_then(|files| files.pointer_mut("/data"))
        .and_then(Value::as_array_mut)
        .unwrap();
    listing.truncate(3);
    if let [kept_off, unpublished, unflagged] = listing.as_mut_slice() {
        kept_off
            .as_object_mut()
            .unwrap()
            .insert("isAvailable".to_owned(), json!(false));
        unpublished
            .as_object_mut()
            .unwrap()
            .insert("downloadUrl".to_owned(), Value::Null);
        unflagged.as_object_mut().unwrap().remove("isAvailable");
    }
    let listed = releases(adapter.as_ref(), &http, &target());
    let download = |id: &str| {
        listed
            .iter()
            .find(|release| release.id == id)
            .and_then(Release::primary_file)
            .unwrap()
            .clone()
    };
    assert_eq!(
        download("501").download,
        Download::UserAction {
            page: "https://www.catalog.test/432/mods/100/files/501".to_owned(),
            reason: ActionReason::DistributionForbidden,
        }
    );
    assert!(matches!(
        download("502").download,
        Download::UserAction {
            reason: ActionReason::NoDownloadUrl,
            ..
        }
    ));
    assert!(matches!(download("503").download, Download::Direct { .. }));

    let dir = tempfile::tempdir().unwrap();
    let refused = adapter
        .acquire(&http, &download("501"), dir.path())
        .unwrap_err();
    assert!(matches!(refused, AdapterError::ActionRequired { .. }));
    assert!(
        refused
            .to_string()
            .contains("https://www.catalog.test/432/mods/100/files/501"),
        "{refused}"
    );
    assert!(
        !http
            .requested()
            .iter()
            .any(|request| request.starts_with("DOWNLOAD"))
    );

    let website_only = program_adapter(
        &PROGRAM
            .replace(
                "type = \"direct_https\"",
                "type = \"browser_assisted\"\nscheme = \"handoff\"",
            )
            .replace("distributable = \"/isAvailable\"\n", "")
            .replace(
                "[mappings]\n",
                "[handoff]\nhost = \"game\"\npath = [\"mods\", \"{project}\", \"files\", \"{release}\"]\nredeem = \"/v1/mods/{project}/files/{release}/link\"\n\n[mappings.handoff]\nurls = \"/url\"\n\n[mappings]\n",
            ),
    );
    let listed = releases(website_only.as_ref(), &http, &target());
    assert!(listed.iter().all(|release| matches!(
        &release.primary_file().unwrap().download,
        Download::BrowserAssisted { scheme, .. } if scheme == "handoff"
    )));
}

#[test]
fn projects_listed_for_another_game_are_refused() {
    let adapter = program_adapter(PROGRAM);
    let http = catalogue();
    let releases = adapter.as_releases().unwrap();
    assert_eq!(
        releases.project(&http, "100", &target()).unwrap().title,
        "Mod"
    );
    let refused = releases.project(&http, "200", &target()).unwrap_err();
    assert!(
        refused.to_string().contains("not listed for game"),
        "{refused}"
    );
    assert!(releases.project(&http, "../100", &target()).is_err());
}

#[test]
fn listed_release_updates_move_forward_on_their_channel_and_back_only_to_fit() {
    let adapter = program_adapter(PROGRAM);
    let http = catalogue();
    let updates = adapter.as_updates().unwrap();
    let check = |installed: Provenance, target: &Target| {
        updates
            .check(&http, &[&installed], target)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    };
    let moved_to = |outcome: UpdateCheck| match outcome {
        UpdateCheck::Available(update) => update.release.id,
        other => panic!("expected an update, got {other:?}"),
    };

    assert_eq!(
        moved_to(check(provenance("501", "1.2.0"), &target())),
        "503"
    );
    assert_eq!(
        check(provenance("503", "1.10.0"), &target()),
        UpdateCheck::Current
    );
    assert_eq!(
        moved_to(check(provenance("502", "1.3.0-beta.1"), &target())),
        "503"
    );
    assert_eq!(
        moved_to(check(provenance("504", "2.0.0"), &target())),
        "503"
    );
    assert_eq!(
        check(provenance("999", "1.11.0"), &target()),
        UpdateCheck::Current
    );
    assert_eq!(
        moved_to(check(provenance("998", "1.0.0"), &target())),
        "503"
    );

    let mut unsupported = target();
    unsupported.game_version = Some("3.0".to_owned());
    assert_eq!(
        check(provenance("501", "1.2.0"), &unsupported),
        UpdateCheck::Incompatible
    );

    let mut escaping = provenance("501", "1.2.0");
    escaping.project = "../100".to_owned();
    let before = http.requested().len();
    assert!(updates.check(&http, &[&escaping], &target()).is_err());
    assert_eq!(http.requested().len(), before);
}
