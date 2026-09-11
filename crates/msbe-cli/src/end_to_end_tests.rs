//! End to end: M1 workflows against a synthetic Minecraft directory, driven through the real
//! command-line surface in-process with the first-party plan. Modrinth is served by an
//! in-memory fake, so these tests never touch the network.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
};

use msbe_providers::{HttpClient, HttpError};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256, Sha512};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{exit, run_with};

const API: &str = "https://api.modrinth.com/v2";
const CDN: &str = "https://cdn.modrinth.test";

/// A map shared between a test and the clones of the fake handed to the CLI.
type Shared<T> = Rc<RefCell<BTreeMap<String, T>>>;

/// An in-memory Modrinth: canned JSON by URL, and hosted files.
#[derive(Clone, Default)]
struct FakeModrinth {
    json: Shared<Value>,
    files: Shared<Vec<u8>>,
}

impl FakeModrinth {
    /// Sodium, and Iris, which requires Sodium.
    fn catalogue() -> Self {
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
        fake.json.borrow_mut().insert(
            format!("{API}/search"),
            json!({ "hits": [{ "project_id": "AANobbMI", "slug": "sodium", "title": "Sodium",
                               "description": "A rendering engine", "downloads": 42 }] }),
        );
        fake
    }

    /// Publishes a version of a project, creating the project the first time. The file is
    /// `<slug>-fabric-<number>.jar`, and its bytes name the project and version.
    fn publish(&self, slug: &str, number: &str, kind: &str, date: &str, dependencies: &Value) {
        let id = project_id(slug);
        let file = format!("{slug}-fabric-{number}.jar");
        let bytes = format!("{slug} {number} jar bytes").into_bytes();
        let version = json!({
            "id": format!("{id}-{number}"),
            "project_id": id,
            "version_number": number,
            "version_type": kind,
            "date_published": date,
            "loaders": ["fabric"],
            "game_versions": ["1.21.1"],
            "files": [{
                "hashes": { "sha512": sha512_hex(&bytes) },
                "url": format!("{CDN}/{file}"),
                "filename": file,
                "primary": true,
                "size": bytes.len()
            }],
            "dependencies": dependencies
        });
        let project = json!({ "id": id, "slug": slug, "title": slug, "project_type": "mod" });
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

    /// Every published version of every project.
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

    /// Flips one bit of a hosted file without changing its size.
    fn corrupt(&self, file: &str) {
        if let Some(bytes) = self.files.borrow_mut().get_mut(&format!("{CDN}/{file}"))
            && let Some(last) = bytes.last_mut()
        {
            *last ^= 1;
        }
    }
}

impl HttpClient for FakeModrinth {
    fn get(&self, url: &str, _query: &[(&str, &str)], _limit: u64) -> Result<Vec<u8>, HttpError> {
        self.json
            .borrow()
            .get(url)
            .map(|body| serde_json::to_vec(body).unwrap())
            .ok_or_else(|| HttpError::Status {
                url: url.to_owned(),
                status: 404,
            })
    }

    /// Answers the bulk lookups from the published versions, as Modrinth does:
    /// `/version_files` finds the version each hash belongs to, and `/version_files/update` the
    /// newest version of that project with a requested type and the requested game version.
    fn post_json(&self, url: &str, body: &[u8], _limit: u64) -> Result<Vec<u8>, HttpError> {
        let request: Value = serde_json::from_slice(body).unwrap();
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
        Ok(serde_json::to_vec(&answer).unwrap())
    }

    fn download(&self, url: &str, sink: &mut dyn Write, _limit: u64) -> Result<u64, HttpError> {
        let files = self.files.borrow();
        let bytes = files.get(url).ok_or_else(|| HttpError::Status {
            url: url.to_owned(),
            status: 404,
        })?;
        sink.write_all(bytes).unwrap();
        Ok(u64::try_from(bytes.len()).unwrap())
    }
}

/// Whether a version has a requested type, if any are requested, and the requested game
/// version.
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

fn project_id(slug: &str) -> &'static str {
    match slug {
        "sodium" => "AANobbMI",
        "iris" => "YL57xq9U",
        _ => panic!("no id for test project {slug}"),
    }
}

fn requires(project: &str) -> Value {
    json!([{ "project_id": project, "version_id": null, "dependency_type": "required" }])
}

fn sha512_hex(bytes: &[u8]) -> String {
    hex(&Sha512::digest(bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").unwrap();
        out
    })
}

struct World {
    _dir: TempDir,
    home: PathBuf,
    game: PathBuf,
    inputs: PathBuf,
    plan: PathBuf,
    modrinth: FakeModrinth,
}

struct Outcome {
    code: u8,
    out: String,
    err: String,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("minecraft");
        fs::create_dir_all(game.join("mods")).unwrap();
        fs::create_dir_all(game.join("saves/world")).unwrap();
        fs::write(game.join("options.txt"), b"fov:70\n").unwrap();
        fs::write(game.join("saves/world/level.dat"), b"level data").unwrap();
        // A mod the player installed by hand. A managed mod will overwrite it, and purge must
        // bring it back.
        fs::write(game.join("mods/lithium.jar"), b"hand-installed lithium").unwrap();
        let inputs = dir.path().join("downloads");
        fs::create_dir_all(&inputs).unwrap();
        Self {
            home: dir.path().join("msbe-home"),
            game,
            inputs,
            plan: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plans/minecraft/plan.toml"),
            modrinth: FakeModrinth::catalogue(),
            _dir: dir,
        }
    }

    fn msbe(&self, args: &[&str]) -> Outcome {
        let mut command_line = vec![
            "msbe".to_owned(),
            "--home".to_owned(),
            self.home.display().to_string(),
        ];
        command_line.extend(args.iter().map(|arg| (*arg).to_owned()));
        let modrinth = self.modrinth.clone();
        let connect =
            move || -> Result<Box<dyn HttpClient>, HttpError> { Ok(Box::new(modrinth.clone())) };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(command_line, &mut out, &mut err, &connect);
        Outcome {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut full = vec!["--format", "json"];
        full.extend_from_slice(args);
        let outcome = self.msbe(&full);
        assert_eq!(outcome.code, exit::OK, "{args:?} failed: {}", outcome.err);
        serde_json::from_str(&outcome.out).unwrap()
    }

    fn add_instance(&self, game_version: Option<&str>) {
        let game = self.game.display().to_string();
        let plan = self.plan.display().to_string();
        let mut args = vec![
            "instance",
            "add",
            "mc",
            "--root",
            game.as_str(),
            "--plan",
            plan.as_str(),
            "--loader",
            "fabric",
        ];
        if let Some(version) = game_version {
            args.extend(["--game-version", version]);
        }
        let outcome = self.msbe(&args);
        assert_eq!(outcome.code, exit::OK, "{}", outcome.err);
    }

    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.inputs.join(name);
        fs::write(&path, bytes).unwrap();
        path.display().to_string()
    }

    fn zip(&self, name: &str, entries: &[(&str, &[u8])]) -> String {
        let path = self.inputs.join(name);
        let mut writer = ZipWriter::new(fs::File::create(&path).unwrap());
        for (entry, bytes) in entries {
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            writer.start_file(*entry, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        path.display().to_string()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Node {
    Dir,
    File(Vec<u8>),
}

fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn walk(root: &Path, dir: &Path, entries: &mut BTreeMap<String, Node>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let key = path.strip_prefix(root).unwrap().display().to_string();
        if path.is_dir() {
            entries.insert(key, Node::Dir);
            walk(root, &path, entries);
        } else {
            entries.insert(key, Node::File(fs::read(&path).unwrap()));
        }
    }
}

/// The value at a JSON pointer, failing the test with the whole document if it is absent.
fn at<'a>(document: &'a Value, pointer: &str) -> &'a Value {
    document
        .pointer(pointer)
        .unwrap_or_else(|| panic!("no {pointer} in {document}"))
}

/// Something outside MSBE deletes a deployed file: verify reports it, and deploying the
/// profile again repairs it.
fn drift_is_reported_by_verify_and_repaired_by_deploy(world: &World) {
    world.json(&["deploy", "mc", "--profile", "lean"]);
    #[expect(
        clippy::disallowed_methods,
        reason = "simulates a file deleted outside MSBE"
    )]
    let deleted = fs::remove_file(world.game.join("mods/lithium.jar"));
    deleted.unwrap();
    let drift = world.msbe(&["--format", "json", "verify", "mc"]);
    assert_eq!(drift.code, exit::INTEGRITY);
    let drift: Value = serde_json::from_str(&drift.out).unwrap();
    assert_eq!(at(&drift, "/missing"), &json!(["mods/lithium.jar"]));
    assert_eq!(
        at(
            &world.json(&["deploy", "mc", "--profile", "lean"]),
            "/placed"
        ),
        1
    );
    assert_eq!(world.msbe(&["verify", "mc"]).code, exit::OK);
}

#[test]
fn local_files_install_switch_repair_roll_back_and_purge_to_vanilla() {
    let world = World::new();
    let vanilla = snapshot(&world.game);
    world.add_instance(Some("1.21.1"));

    let sodium = world.file("sodium-0.6.0.jar", b"sodium bytes");
    let lithium = world.file("lithium.jar", b"managed lithium");
    let pack = world.zip(
        "shader-pack.zip",
        &[
            ("jars/iris-1.8.jar", b"iris bytes"),
            ("README.md", b"read me"),
            ("__MACOSX/._iris-1.8.jar", b"resource fork"),
        ],
    );
    let added = world.json(&[
        "add",
        "mc",
        sodium.as_str(),
        lithium.as_str(),
        pack.as_str(),
    ]);
    assert_eq!(
        at(&added, "/added"),
        &json!(["sodium-0.6.0", "lithium", "shader-pack"])
    );

    // A dry run changes nothing and explains every file it leaves out.
    let dry_run = world.json(&["deploy", "mc", "--dry-run"]);
    assert_eq!(
        at(&dry_run, "/operations").as_array().map(Vec::len),
        Some(3)
    );
    let excluded = at(&dry_run, "/excluded").as_array().unwrap();
    let reason = |source: &str| {
        excluded
            .iter()
            .find(|item| at(item, "/file/source") == source)
            .map(|item| at(item, "/file/reason/kind").clone())
    };
    assert_eq!(reason("README.md"), Some(json!("not_allowed")));
    assert_eq!(reason("__MACOSX/._iris-1.8.jar"), Some(json!("hygiene")));
    assert_eq!(
        snapshot(&world.game),
        vanilla,
        "a dry run touched the instance"
    );

    // Deploy the default profile.
    let deployed = world.json(&["deploy", "mc"]);
    assert_eq!(at(&deployed, "/placed"), 3);
    assert_eq!(
        fs::read(world.game.join("mods/iris-1.8.jar")).unwrap(),
        b"iris bytes"
    );
    assert_eq!(
        fs::read(world.game.join("mods/lithium.jar")).unwrap(),
        b"managed lithium"
    );
    assert!(!world.game.join("mods/README.md").exists());

    // Switch to a lean profile: two files go, lithium is left alone.
    assert_eq!(world.msbe(&["profile", "new", "mc", "lean"]).code, exit::OK);
    world.json(&["add", "mc", "--profile", "lean", lithium.as_str()]);
    let switched = world.json(&["deploy", "mc", "--profile", "lean"]);
    assert_eq!(
        [
            at(&switched, "/placed"),
            at(&switched, "/removed"),
            at(&switched, "/unchanged")
        ],
        [&json!(0), &json!(2), &json!(1)]
    );
    assert!(!world.game.join("mods/sodium-0.6.0.jar").exists());

    // Rolling back the switch brings the default profile's files back.
    assert!(at(&world.json(&["rollback", "mc"]), "/rolled_back").is_number());
    assert!(world.game.join("mods/sodium-0.6.0.jar").exists());
    assert_eq!(at(&world.json(&["verify", "mc"]), "/checked"), 3);

    drift_is_reported_by_verify_and_repaired_by_deploy(&world);

    // Purge undoes every live deployment and restores the directory byte for byte,
    // including the hand-installed mod that a managed file overwrote.
    let purged = world.json(&["purge", "mc"]);
    assert_eq!(
        at(&purged, "/rolled_back").as_array().map(Vec::len),
        Some(3)
    );
    assert_eq!(
        snapshot(&world.game),
        vanilla,
        "purge did not restore vanilla"
    );
    let status = world.json(&["status", "mc"]);
    assert_eq!(at(&status, "/deployed_files"), 0);
    assert_eq!(at(&status, "/live_transactions"), 0);
}

#[test]
fn modrinth_mods_install_with_dependencies_record_provenance_and_purge_cleanly() {
    let world = World::new();
    let vanilla = snapshot(&world.game);
    world.add_instance(Some("1.21.1"));

    let added = world.json(&["add", "mc", "modrinth:iris", "--with-deps"]);
    assert_eq!(at(&added, "/added"), &json!(["iris", "sodium"]));
    assert_eq!(at(&added, "/unresolved"), &json!([]));

    let profile = world.json(&["profile", "show", "mc"]);
    assert_eq!(at(&profile, "/mods/sodium/provider/provider"), "modrinth");
    assert_eq!(at(&profile, "/mods/sodium/provider/project"), "AANobbMI");
    assert_eq!(
        at(&profile, "/mods/sodium/provider/version_number"),
        "0.8.12"
    );

    // Asking again skips what the profile already has.
    let again = world.json(&["add", "mc", "modrinth:sodium"]);
    assert_eq!(at(&again, "/added"), &json!([]));
    assert_eq!(at(&again, "/skipped"), &json!(["sodium"]));

    assert_eq!(at(&world.json(&["deploy", "mc"]), "/placed"), 2);
    assert_eq!(
        fs::read(world.game.join("mods/sodium-fabric-0.8.12.jar")).unwrap(),
        b"sodium 0.8.12 jar bytes"
    );
    world.json(&["purge", "mc"]);
    assert_eq!(snapshot(&world.game), vanilla);
}

#[test]
fn update_moves_modrinth_mods_forward_on_their_channel_and_deploys_like_any_change() {
    let world = World::new();
    let vanilla = snapshot(&world.game);
    world.add_instance(Some("1.21.1"));
    let lithium = world.file("lithium.jar", b"managed lithium");
    world.json(&[
        "add",
        "mc",
        "modrinth:iris",
        "--with-deps",
        lithium.as_str(),
    ]);
    world.json(&["deploy", "mc"]);

    // A newer release and an even newer beta: a mod installed from a release takes the release.
    let modrinth = &world.modrinth;
    modrinth.publish(
        "sodium",
        "0.8.13",
        "release",
        "2026-08-28T00:00:00Z",
        &json!([]),
    );
    modrinth.publish(
        "sodium",
        "0.9.0-beta.1",
        "beta",
        "2026-09-01T00:00:00Z",
        &json!([]),
    );
    // Iris's next release needs a project the profile does not have.
    modrinth.publish(
        "iris",
        "1.9.0",
        "release",
        "2026-08-30T00:00:00Z",
        &requires("P7dR8mSH"),
    );

    let preview = world.json(&["update", "mc", "--dry-run"]);
    assert_eq!(
        at(&preview, "/updated"),
        &json!([
            { "module": "iris", "from": "1.8.0", "to": "1.9.0" },
            { "module": "sodium", "from": "0.8.12", "to": "0.8.13" }
        ])
    );
    assert_eq!(at(&preview, "/not_from_modrinth"), &json!(["lithium"]));
    assert_eq!(
        at(&preview, "/unresolved"),
        &json!([{ "project_id": "P7dR8mSH", "declared_by": "iris" }])
    );
    let show = || world.json(&["profile", "show", "mc"]);
    assert_eq!(
        at(&show(), "/mods/sodium/provider/version_number"),
        "0.8.12"
    );

    // Update only Sodium, for real.
    let updated = world.msbe(&["update", "mc", "sodium"]);
    assert_eq!(updated.code, exit::OK, "{}", updated.err);
    assert!(
        updated.out.contains("sodium  0.8.12 -> 0.8.13"),
        "{}",
        updated.out
    );
    assert_eq!(
        at(&show(), "/mods/sodium/provider/version_number"),
        "0.8.13"
    );
    assert_eq!(at(&show(), "/mods/iris/provider/version_number"), "1.8.0");
    assert_eq!(
        at(&world.json(&["update", "mc", "sodium"]), "/current"),
        &json!(["sodium"])
    );

    let deployed = world.json(&["deploy", "mc"]);
    assert_eq!(
        [at(&deployed, "/placed"), at(&deployed, "/removed")],
        [&json!(1), &json!(1)]
    );
    assert_eq!(
        fs::read(world.game.join("mods/sodium-fabric-0.8.13.jar")).unwrap(),
        b"sodium 0.8.13 jar bytes"
    );
    assert!(!world.game.join("mods/sodium-fabric-0.8.12.jar").exists());
    assert_eq!(world.msbe(&["verify", "mc"]).code, exit::OK);

    world.json(&["rollback", "mc"]);
    assert!(world.game.join("mods/sodium-fabric-0.8.12.jar").exists());
    world.json(&["purge", "mc"]);
    assert_eq!(snapshot(&world.game), vanilla);

    let unknown = world.msbe(&["update", "mc", "nonexistent"]);
    assert_eq!(unknown.code, exit::FAILURE);
    assert!(
        unknown.err.contains("no mod named nonexistent"),
        "{}",
        unknown.err
    );
}

#[test]
fn without_dependencies_a_missing_requirement_is_reported_not_installed() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let added = world.json(&["add", "mc", "modrinth:iris"]);
    assert_eq!(at(&added, "/added"), &json!(["iris"]));
    assert_eq!(
        at(&added, "/unresolved"),
        &json!([{ "project_id": "AANobbMI", "declared_by": "iris" }])
    );
}

#[test]
fn a_download_failing_verification_adds_nothing() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    world.modrinth.corrupt("sodium-fabric-0.8.12.jar");

    let outcome = world.msbe(&["add", "mc", "modrinth:sodium"]);
    assert_eq!(outcome.code, exit::FAILURE);
    assert!(outcome.err.contains("SHA-512"), "{}", outcome.err);
    assert_eq!(
        at(&world.json(&["profile", "show", "mc"]), "/mods"),
        &json!({})
    );
}

#[test]
fn modrinth_needs_a_game_version_which_can_be_set_later() {
    let world = World::new();
    world.add_instance(None);

    let outcome = world.msbe(&["add", "mc", "modrinth:sodium"]);
    assert_eq!(outcome.code, exit::FAILURE);
    assert!(outcome.err.contains("--game-version"), "{}", outcome.err);

    let updated = world.json(&["instance", "set", "mc", "--game-version", "1.21.1"]);
    assert_eq!(at(&updated, "/game_version"), "1.21.1");
    assert_eq!(
        at(&world.json(&["add", "mc", "modrinth:sodium"]), "/added"),
        &json!(["sodium"])
    );
}

#[test]
fn search_lists_mods_compatible_with_the_instance() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let hits = world.json(&["search", "mc", "rendering", "engine"]);
    assert_eq!(at(&hits, "/0/slug"), "sodium");
}

#[test]
fn conflicting_mods_exit_with_the_conflict_code_and_change_nothing() {
    let world = World::new();
    let vanilla = snapshot(&world.game);
    world.add_instance(Some("1.21.1"));
    let first = world.zip("pack-a.zip", &[("a/common.jar", b"version a")]);
    let second = world.zip("pack-b.zip", &[("b/common.jar", b"version b")]);
    world.json(&["add", "mc", first.as_str(), second.as_str()]);

    let outcome = world.msbe(&["deploy", "mc"]);
    assert_eq!(outcome.code, exit::CONFLICT);
    assert!(outcome.err.contains("mods/common.jar"), "{}", outcome.err);
    assert_eq!(snapshot(&world.game), vanilla);
}

#[test]
fn usage_errors_and_bad_references_have_distinct_exit_codes() {
    let world = World::new();
    assert_eq!(world.msbe(&["deploy"]).code, exit::USAGE);

    let unknown = world.msbe(&["status", "nope"]);
    assert_eq!(unknown.code, exit::FAILURE);
    assert!(
        unknown.err.contains("no instance named nope"),
        "{}",
        unknown.err
    );

    let game = world.game.display().to_string();
    let plan = world.plan.display().to_string();
    let bad_loader = world.msbe(&[
        "instance",
        "add",
        "mc",
        "--root",
        game.as_str(),
        "--plan",
        plan.as_str(),
        "--loader",
        "forge-1.7",
    ]);
    assert_eq!(bad_loader.code, exit::FAILURE);
    assert!(bad_loader.err.contains("forge-1.7"), "{}", bad_loader.err);

    world.add_instance(Some("1.21.1"));
    let bad_reference = world.msbe(&["add", "mc", "modrinth:../etc/passwd"]);
    assert_eq!(bad_reference.code, exit::FAILURE);
    assert!(
        bad_reference.err.contains("not a valid Modrinth"),
        "{}",
        bad_reference.err
    );
}

#[test]
fn direct_urls_install_with_pinned_checksums_and_refuse_plain_http() {
    let world = World::new();
    // A direct download needs no game version.
    world.add_instance(None);
    let url = "https://files.example.test/mods/extra-1.0.jar";
    let bytes = b"extra mod bytes";
    world
        .modrinth
        .files
        .borrow_mut()
        .insert(url.to_owned(), bytes.to_vec());
    let mods = || at(&world.json(&["profile", "show", "mc"]), "/mods").clone();

    let wrong = format!("{url}#sha256={}", "0".repeat(64));
    let refused = world.msbe(&["add", "mc", wrong.as_str()]);
    assert_eq!(refused.code, exit::FAILURE);
    assert!(refused.err.contains("SHA-256"), "{}", refused.err);
    let insecure = world.msbe(&["add", "mc", "http://files.example.test/mods/extra-1.0.jar"]);
    assert_eq!(insecure.code, exit::FAILURE);
    assert!(insecure.err.contains("insecure"), "{}", insecure.err);
    assert_eq!(mods(), json!({}));

    let pinned = format!("{url}#sha256={}", sha256_hex(bytes));
    let added = world.json(&["add", "mc", pinned.as_str()]);
    assert_eq!(at(&added, "/added"), &json!(["extra-1.0"]));
    let installed = mods();
    assert_eq!(at(&installed, "/extra-1.0/provider/provider"), "url");
    assert_eq!(at(&installed, "/extra-1.0/provider/project"), url);
    assert_eq!(
        at(&installed, "/extra-1.0/provider/sha512"),
        sha512_hex(bytes).as_str()
    );
    assert_eq!(
        at(&world.json(&["add", "mc", url]), "/skipped"),
        &json!(["extra-1.0"])
    );

    world.json(&["deploy", "mc"]);
    assert_eq!(
        fs::read(world.game.join("mods/extra-1.0.jar")).unwrap(),
        bytes
    );
}
