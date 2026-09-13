//! End to end: M1 workflows against a synthetic Minecraft directory, driven through the real
//! command-line surface in-process with the first-party plan. Modrinth is served by an
//! in-memory fake, so these tests never touch the network.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use msbe_provider_api::{HttpClient, HttpError};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256, Sha512};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{exit, fake_modrinth::FakeModrinth, run_with};

const API: &str = "https://api.modrinth.com/v2";

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
        self.add_instance_with_loader("fabric", game_version);
    }

    fn add_instance_with_loader(&self, loader: &str, game_version: Option<&str>) {
        self.add_instance_with_loader_and_side(loader, "client", game_version);
    }

    fn add_instance_with_loader_and_side(
        &self,
        loader: &str,
        side: &str,
        game_version: Option<&str>,
    ) {
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
            loader,
            "--side",
            side,
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
fn pack_export_writes_verified_profile_files_as_overrides() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let artifact = world.file("sodium.jar", b"verified sodium");
    world.json(&["add", "mc", artifact.as_str()]);
    let output = world.inputs.join("profile.mrpack");
    let output_text = output.display().to_string();

    let report = world.json(&[
        "pack",
        "export",
        "mc",
        output_text.as_str(),
        "--codec",
        "modrinth-mrpack",
    ]);
    assert_eq!(at(&report, "/embedded"), 1);
    assert_eq!(at(&report, "/output"), &json!(output));

    let mut archive = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
    let mut index = String::new();
    archive
        .by_name("modrinth.index.json")
        .unwrap()
        .read_to_string(&mut index)
        .unwrap();
    let index: Value = serde_json::from_str(&index).unwrap();
    assert_eq!(at(&index, "/dependencies/minecraft"), "1.21.1");
    let mut exported = Vec::new();
    archive
        .by_name("overrides/mods/sodium.jar")
        .unwrap()
        .read_to_end(&mut exported)
        .unwrap();
    assert_eq!(exported, b"verified sodium");
}

#[test]
fn pack_configs_can_be_edited_validated_and_exported() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));

    world.json(&[
        "pack",
        "config",
        "set",
        "mc",
        "config/example.toml",
        "--content",
        "enabled = false\n",
    ]);
    world.json(&[
        "pack",
        "config",
        "set",
        "mc",
        "config/example.toml",
        "--content",
        "enabled = true\n",
    ]);
    let listed = world.json(&["pack", "config", "list", "mc"]);
    assert_eq!(at(&listed, "/0/path"), "config/example.toml");
    let shown = world.json(&["pack", "config", "show", "mc", "config/example.toml"]);
    assert_eq!(at(&shown, "/content"), "enabled = true\n");

    let validated = world.json(&["pack", "validate", "mc"]);
    assert_eq!(at(&validated, "/configs"), 1);
    assert_eq!(at(&validated, "/files"), 1);
    assert!(Path::new(at(&validated, "/lockfile").as_str().unwrap()).is_file());

    let output = world.inputs.join("configured.mrpack");
    world.json(&[
        "pack",
        "export",
        "mc",
        output.to_str().unwrap(),
        "--codec",
        "modrinth-mrpack",
    ]);
    let mut archive = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
    let mut exported = String::new();
    archive
        .by_name("overrides/config/example.toml")
        .unwrap()
        .read_to_string(&mut exported)
        .unwrap();
    assert_eq!(exported, "enabled = true\n");

    world.json(&["pack", "config", "remove", "mc", "config/example.toml"]);
    assert_eq!(world.json(&["pack", "config", "list", "mc"]), json!([]));
}

#[test]
fn pack_import_acquires_target_compatible_verified_modrinth_files() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let url = "https://cdn.modrinth.test/imported.jar";
    let bytes = b"imported bytes";
    world
        .modrinth
        .files
        .borrow_mut()
        .insert(url.to_owned(), bytes.to_vec());
    let index = json!({
        "formatVersion": 1,
        "files": [{
            "path": "mods/imported.jar",
            "downloads": [url],
            "hashes": { "sha512": sha512_hex(bytes) },
            "env": { "client": "required", "server": "unsupported" }
        }]
    });
    let pack = world.zip(
        "import.mrpack",
        &[(
            "modrinth.index.json",
            serde_json::to_string(&index).unwrap().as_bytes(),
        )],
    );

    let imported = world.json(&["pack", "import", "mc", pack.as_str()]);
    assert_eq!(at(&imported, "/added"), &json!(["imported"]));
    assert_eq!(world.msbe(&["deploy", "mc"]).code, exit::OK);
    assert_eq!(
        fs::read(world.game.join("mods/imported.jar")).unwrap(),
        bytes
    );
}

#[test]
fn pack_import_preserves_zip_assets_at_their_declared_targets() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let resource_archive = world.zip(
        "Better-Leaves.zip",
        &[("assets/example/leaves.png", b"resource pack")],
    );
    let shader_archive = world.zip(
        "Complementary.zip",
        &[("shaders/example.glsl", b"shader pack")],
    );
    let resource_bytes = fs::read(&resource_archive).unwrap();
    let shader_bytes = fs::read(&shader_archive).unwrap();
    let resource_url = "https://cdn.modrinth.test/Better-Leaves.zip";
    let shader_url = "https://cdn.modrinth.test/Complementary.zip";
    world.modrinth.files.borrow_mut().extend([
        (resource_url.to_owned(), resource_bytes.clone()),
        (shader_url.to_owned(), shader_bytes.clone()),
    ]);
    let index = json!({
        "formatVersion": 1,
        "files": [
            {
                "path": "resourcepacks/Better-Leaves.zip",
                "downloads": [resource_url],
                "hashes": { "sha512": sha512_hex(&resource_bytes) },
                "env": { "client": "required", "server": "unsupported" }
            },
            {
                "path": "shaderpacks/Complementary.zip",
                "downloads": [shader_url],
                "hashes": { "sha512": sha512_hex(&shader_bytes) },
                "env": { "client": "required", "server": "unsupported" }
            }
        ]
    });
    let pack = world.zip(
        "assets.mrpack",
        &[(
            "modrinth.index.json",
            serde_json::to_string(&index).unwrap().as_bytes(),
        )],
    );

    world.json(&["pack", "import", "mc", pack.as_str()]);
    assert_eq!(world.msbe(&["deploy", "mc"]).code, exit::OK);
    assert_eq!(
        fs::read(world.game.join("resourcepacks/Better-Leaves.zip")).unwrap(),
        resource_bytes
    );
    assert_eq!(
        fs::read(world.game.join("shaderpacks/Complementary.zip")).unwrap(),
        shader_bytes
    );
    assert!(!world.game.join("mods/leaves.png").exists());
}

#[test]
fn profile_remove_deletes_inactive_profiles_but_protects_active_ones() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    assert_eq!(
        world.msbe(&["profile", "new", "mc", "temporary"]).code,
        exit::OK
    );
    assert_eq!(
        world.msbe(&["profile", "remove", "mc", "temporary"]).code,
        exit::OK
    );
    world.json(&["deploy", "mc"]);
    let rejected = world.msbe(&["profile", "remove", "mc", "default"]);
    assert_eq!(rejected.code, exit::FAILURE);
    assert!(
        rejected.err.contains("currently deployed"),
        "{}",
        rejected.err
    );
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
    assert_eq!(at(&preview, "/not_updatable"), &json!(["lithium"]));
    assert_eq!(
        at(&preview, "/unresolved"),
        &json!([{ "provider": "modrinth", "project_id": "P7dR8mSH", "declared_by": "iris" }])
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
        &json!([{ "provider": "modrinth", "project_id": "AANobbMI", "declared_by": "iris" }])
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
fn providers_serve_an_instance_without_a_game_version_and_one_can_be_set_later() {
    let world = World::new();
    world.add_instance(None);

    assert_eq!(
        at(&world.json(&["add", "mc", "modrinth:iris"]), "/added"),
        &json!(["iris"])
    );

    let updated = world.json(&["instance", "set", "mc", "--game-version", "1.21.1"]);
    assert_eq!(at(&updated, "/game_version"), "1.21.1");
    assert_eq!(
        at(&world.json(&["add", "mc", "modrinth:sodium"]), "/added"),
        &json!(["sodium"])
    );

    let edition = world.msbe(&["instance", "set", "mc", "--edition", "remaster"]);
    assert_eq!(edition.code, exit::FAILURE);
    assert!(
        edition.err.contains("declares no edition"),
        "{}",
        edition.err
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
fn profile_target_persists_and_quilt_provides_fabric_compatibility() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    world.json(&["profile", "new", "mc", "performance"]);
    let target = world.json(&[
        "profile",
        "set-target",
        "mc",
        "performance",
        "--loader",
        "quilt",
        "--side",
        "client",
    ]);
    assert_eq!(at(&target, "/loader"), "quilt");
    assert_eq!(at(&target, "/side"), "client");

    assert_eq!(
        at(
            &world.json(&["add", "mc", "modrinth:sodium", "--profile", "performance"]),
            "/added",
        ),
        &json!(["sodium"])
    );
    assert_eq!(
        at(
            &world.json(&["profile", "show", "mc", "performance"]),
            "/target/loader"
        ),
        "quilt"
    );
}

#[test]
fn a_fabric_api_reimplementation_in_the_profile_meets_requirements_on_fabric_api() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let date = "2026-08-01T00:00:00Z";
    world
        .modrinth
        .publish("fabric-api", "0.116.0", "release", date, &json!([]));
    world
        .modrinth
        .publish("qsl", "10.0.0", "release", date, &json!([]));
    world
        .modrinth
        .publish("sodium", "0.9.0", "release", date, &requires("P7dR8mSH"));

    // With nothing standing in for it, Fabric API itself meets the requirement.
    let fabric = world.json(&["add", "mc", "modrinth:sodium", "--with-deps"]);
    assert_eq!(at(&fabric, "/added"), &json!(["sodium", "fabric-api"]));
    assert_eq!(at(&fabric, "/substituted"), &json!([]));

    // Quilted Fabric API provides Fabric API, so a profile that has it gets no second copy.
    world.json(&["profile", "new", "mc", "quilt"]);
    world.json(&[
        "profile",
        "set-target",
        "mc",
        "quilt",
        "--loader",
        "quilt",
        "--side",
        "client",
    ]);
    world.json(&["add", "mc", "modrinth:qsl", "--profile", "quilt"]);
    let quilt = world.json(&[
        "add",
        "mc",
        "modrinth:sodium",
        "--with-deps",
        "--profile",
        "quilt",
    ]);
    assert_eq!(at(&quilt, "/added"), &json!(["sodium"]));
    assert_eq!(
        at(&quilt, "/substituted"),
        &json!([{
            "provider": "modrinth", "project_id": "P7dR8mSH", "declared_by": "sodium",
            "supplied_by": { "provider": "modrinth", "project": "qvIfYCYJ" }
        }])
    );

    // Nor can Fabric API be added beside it.
    let clash = world.msbe(&["add", "mc", "modrinth:fabric-api", "--profile", "quilt"]);
    assert_eq!(clash.code, exit::FAILURE, "{}", clash.out);
    assert!(clash.err.contains("P7dR8mSH"), "{}", clash.err);
    assert_eq!(
        at(&world.json(&["profile", "show", "mc", "quilt"]), "/mods")
            .as_object()
            .map(|mods| mods.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["qsl".to_owned(), "sodium".to_owned()])
    );
}

/// A zip archive of `entries`, stored uncompressed.
fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (entry, bytes) in entries {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file(*entry, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Every entry of the zip archive at `path`, by name.
fn zip_entries(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut entry = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (entry.name().to_owned(), bytes)
        })
        .collect()
}

/// Installs a vanilla 1.5.2 client into `game`, as the launcher lays it out: a signed jar and
/// its version manifest. Returns the jar's bytes.
fn install_vanilla_1_5_2(game: &Path) -> Vec<u8> {
    let version = game.join("versions/1.5.2");
    fs::create_dir_all(&version).unwrap();
    let jar = zip_bytes(&[
        ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
        ("META-INF/MOJANG_C.SF", b"signature"),
        ("a.class", b"vanilla a"),
        ("b.class", b"vanilla b"),
    ]);
    fs::write(version.join("1.5.2.jar"), &jar).unwrap();
    let manifest = json!({
        "id": "1.5.2",
        "mainClass": "net.minecraft.client.Minecraft",
        "downloads": {
            "client": { "sha1": "c", "url": "https://example.test/client.jar" },
            "server": { "sha1": "s", "url": "https://example.test/server.jar" }
        }
    });
    fs::write(
        version.join("1.5.2.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    jar
}

#[test]
fn jarmods_build_a_separate_launcher_version_in_profile_order_and_purge_to_vanilla() {
    let world = World::new();
    let vanilla_jar = install_vanilla_1_5_2(&world.game);
    let vanilla_version = world.game.join("versions/1.5.2");
    let vanilla = snapshot(&world.game);

    let game = world.game.display().to_string();
    let plan = world.plan.display().to_string();
    let registered = world.msbe(&[
        "instance",
        "add",
        "mc",
        "--root",
        game.as_str(),
        "--plan",
        plan.as_str(),
        "--loader",
        "jarmod",
        "--game-version",
        "1.5.2",
    ]);
    assert_eq!(registered.code, exit::OK, "{}", registered.err);

    let modloader = world.zip(
        "modloader.zip",
        &[
            ("a.class", b"modloader a"),
            ("ModLoader.class", b"modloader"),
        ],
    );
    let optifine = world.zip(
        "optifine.zip",
        &[
            ("a.class", b"optifine a"),
            ("Config.class", b"optifine"),
            ("__MACOSX/._Config.class", b"resource fork"),
        ],
    );
    world.json(&["add", "mc", modloader.as_str(), optifine.as_str()]);

    // Added last, OptiFine applies last and wins the class both jarmods replace.
    let modded = world.game.join("versions/1.5.2-msbe");
    assert_eq!(at(&world.json(&["deploy", "mc"]), "/placed"), 2);
    let entries = zip_entries(&modded.join("1.5.2-msbe.jar"));
    assert_eq!(
        entries.keys().map(String::as_str).collect::<Vec<_>>(),
        ["Config.class", "ModLoader.class", "a.class", "b.class"]
    );
    assert_eq!(
        entries.get("a.class").map(Vec::as_slice),
        Some(&b"optifine a"[..])
    );
    assert_eq!(
        entries.get("b.class").map(Vec::as_slice),
        Some(&b"vanilla b"[..])
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(modded.join("1.5.2-msbe.json")).unwrap()).unwrap();
    assert_eq!(at(&manifest, "/id"), "1.5.2-msbe");
    assert!(manifest.pointer("/downloads/client").is_none());
    assert_eq!(at(&manifest, "/downloads/server/sha1"), "s");
    assert_eq!(
        fs::read(vanilla_version.join("1.5.2.jar")).unwrap(),
        vanilla_jar,
        "the vanilla jar was modified"
    );

    // The same inputs build the same bytes, so deploying again changes nothing.
    let again = world.json(&["deploy", "mc"]);
    assert_eq!(
        [at(&again, "/placed"), at(&again, "/unchanged")],
        [&json!(0), &json!(2)]
    );

    // Reordering changes which jarmod wins, and only the jar is rebuilt.
    assert_eq!(
        world.json(&["profile", "order", "mc", "optifine", "modloader"]),
        json!(["optifine", "modloader"])
    );
    assert_eq!(
        at(&world.json(&["profile", "show", "mc"]), "/order"),
        &json!(["optifine", "modloader"])
    );
    assert_eq!(at(&world.json(&["deploy", "mc"]), "/placed"), 1);
    assert_eq!(
        zip_entries(&modded.join("1.5.2-msbe.jar"))
            .get("a.class")
            .map(Vec::as_slice),
        Some(&b"modloader a"[..])
    );

    world.json(&["purge", "mc"]);
    assert_eq!(
        snapshot(&world.game),
        vanilla,
        "purge did not restore vanilla"
    );
}

#[test]
fn server_profile_filters_client_only_projects_before_selection() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let sodium = project_id("sodium");
    for project in [
        format!("{API}/project/sodium"),
        format!("{API}/project/{sodium}"),
    ] {
        world.modrinth.json.borrow_mut().get_mut(&project).unwrap()["server_side"] =
            json!("unsupported");
    }
    world.modrinth.json.borrow_mut().insert(
        format!("{API}/search"),
        json!({ "hits": [{ "project_id": sodium, "slug": "sodium", "title": "Sodium",
                           "description": "A rendering engine", "downloads": 42,
                           "client_side": "required", "server_side": "unsupported" }] }),
    );

    world.json(&[
        "profile",
        "set-target",
        "mc",
        "--loader",
        "fabric",
        "--side",
        "server",
    ]);
    let outcome = world.msbe(&["add", "mc", "modrinth:sodium"]);
    assert_eq!(outcome.code, exit::FAILURE);
    assert!(
        outcome.err.contains("does not support the Server target"),
        "{}",
        outcome.err
    );
    assert_eq!(
        world.json(&["search", "mc", "rendering", "--profile", "default"]),
        json!([])
    );
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
fn forge_routes_explicit_coremods_and_preserves_cfg_files() {
    let world = World::new();
    world.add_instance_with_loader("forge", Some("1.7.10"));
    let forge_pack = world.zip(
        "forge-pack.zip",
        &[
            ("mods/utility.jar", b"regular forge mod"),
            ("coremods/transformer.jar", b"asm transformer"),
            ("config/example.cfg", b"enabled=true\n"),
        ],
    );

    world.json(&["add", "mc", forge_pack.as_str()]);
    world.json(&["deploy", "mc"]);
    assert_eq!(
        fs::read(world.game.join("mods/utility.jar")).unwrap(),
        b"regular forge mod"
    );
    assert_eq!(
        fs::read(world.game.join("coremods/transformer.jar")).unwrap(),
        b"asm transformer"
    );
    assert!(!world.game.join("mods/transformer.jar").exists());
    let config = world.game.join("config/example.cfg");
    fs::write(&config, b"enabled=false\n").unwrap();
    world.json(&["deploy", "mc"]);
    assert_eq!(fs::read(&config).unwrap(), b"enabled=false\n");

    world.json(&["purge", "mc"]);
    assert!(!world.game.join("mods/utility.jar").exists());
    assert!(!world.game.join("coremods/transformer.jar").exists());
    assert!(!config.exists());
}

#[test]
fn paper_routes_plugins_to_the_server_plugin_directory() {
    let world = World::new();
    world.add_instance_with_loader_and_side("paper", "server", Some("1.21.1"));
    let plugins = world.zip(
        "plugins.zip",
        &[
            ("plugins/essentials.jar", b"paper plugin"),
            ("Docs/readme.md", b"not a plugin"),
        ],
    );

    world.json(&["add", "mc", plugins.as_str()]);
    world.json(&["deploy", "mc"]);
    assert_eq!(
        fs::read(world.game.join("plugins/essentials.jar")).unwrap(),
        b"paper plugin"
    );
    assert!(!world.game.join("mods/essentials.jar").exists());

    world.json(&["purge", "mc"]);
    assert!(!world.game.join("plugins/essentials.jar").exists());
}

#[test]
fn no_mans_sky_plan_preserves_local_mod_source_trees_without_core_changes() {
    let mut world = World::new();
    world.plan = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plans/nomanssky/plan.toml");
    world.add_instance_with_loader("none", None);
    let archive = world.zip(
        "nms-mod.zip",
        &[
            ("release/EXAMPLE.pak", b"nms mod"),
            ("release/Example.lua", b"NMS_MOD_DEFINITION_CONTAINER = {}"),
            ("release/readme.txt", b"not a mod input"),
        ],
    );

    world.json(&["add", "mc", archive.as_str()]);
    world.json(&["deploy", "mc"]);
    let pak = world.game.join("GAMEDATA/MODS/release/EXAMPLE.pak");
    assert_eq!(fs::read(&pak).unwrap(), b"nms mod");
    assert_eq!(
        fs::read(world.game.join("GAMEDATA/MODS/release/Example.lua")).unwrap(),
        b"NMS_MOD_DEFINITION_CONTAINER = {}"
    );
    assert!(!world.game.join("GAMEDATA/MODS/release/readme.txt").exists());

    world.json(&["purge", "mc"]);
    assert!(!pak.exists());
    assert!(
        !world
            .game
            .join("GAMEDATA/MODS/release/Example.lua")
            .exists()
    );
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
        at(&installed, "/extra-1.0/provider/hashes/sha512"),
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

/// The deployment digest map `lock` reports for a profile.
fn deployment(world: &World, instance: &str, profile: &str) -> Value {
    at(
        &world.json(&["lock", instance, "--profile", profile]),
        "/deployment",
    )
    .clone()
}

/// A Modrinth pack whose files the fake CDN serves under `version`.
#[test]
fn run_extension_steps_install_from_answers_recorded_on_the_profile() {
    const INSTALLER: &[u8] =
        include_bytes!("../../msbe-plan-host/tests/fixtures/option-installer.wasm");
    let world = World::new();
    let plans = world.inputs.join("installer-plan");
    fs::create_dir_all(&plans).unwrap();
    fs::write(plans.join("installer.wasm"), INSTALLER).unwrap();
    let sha256 = msbe_fsops::Digest::of_bytes(INSTALLER).to_string();
    let plan = plans.join("plan.toml");
    fs::write(
        &plan,
        format!(
            r#"schema = 1
id = "installer"
name = "Installer"
version = "1.0.0"

[[extensions]]
id = "installer"
path = "installer.wasm"
sha256 = "{}"
capabilities = ["archive-read", "game-read", "ui-prompt"]
game_read = ["Data/*.esm"]
emit = ["place", "write-file"]

[[loaders]]
id = "default"
bootstrap = "none"
targets = [{{ name = "mods", path = "mods" }}]

[[steps]]
type = "run-extension"

[steps.with]
id = "install"
extension = "installer"
"#,
            sha256.trim_start_matches("sha256:")
        ),
    )
    .unwrap();
    let game = world.game.display().to_string();
    let plan = plan.display().to_string();
    let added = world.msbe(&[
        "instance",
        "add",
        "ext",
        "--root",
        game.as_str(),
        "--plan",
        plan.as_str(),
        "--loader",
        "default",
    ]);
    assert_eq!(added.code, exit::OK, "{}", added.err);

    let manifest = br#"{"format":"option-installer","version":1,"always":"core","groups":[
        {"id":"textures","prompt":"Textures","default":"standard","options":[
            {"id":"standard","label":"Standard","directory":"options/standard"},
            {"id":"high","label":"High","directory":"options/high"}]}]}"#;
    let archive = world.zip(
        "pack.zip",
        &[
            ("installer.json", manifest),
            ("core/readme.txt", b"core"),
            ("options/standard/texture.dds", b"standard"),
            ("options/high/texture.dds", b"high"),
        ],
    );
    world.json(&["add", "ext", archive.as_str()]);
    world.json(&["deploy", "ext"]);
    assert_eq!(
        fs::read(world.game.join("mods/texture.dds")).unwrap(),
        b"standard"
    );

    let refused = world.msbe(&["profile", "answer", "ext", "pack", "textures=high"]);
    assert_eq!(refused.code, exit::FAILURE);
    assert!(
        refused.err.contains("STEP/QUESTION=ANSWER"),
        "{}",
        refused.err
    );
    let answered = world.json(&["profile", "answer", "ext", "pack", "install/textures=high"]);
    assert_eq!(at(&answered, "/install/textures"), "high");
    world.json(&["deploy", "ext"]);
    assert_eq!(
        fs::read(world.game.join("mods/texture.dds")).unwrap(),
        b"high"
    );
    assert_eq!(
        fs::read_to_string(world.game.join("mods/pack.choices.txt")).unwrap(),
        "textures=high\n"
    );
}

/// A publisher signs a codec with the extension tools; a user trusts the key, verifies the codec,
/// installs it, and MSBE serves it only while the trust root names its signer.
#[test]
fn signed_wasm_codecs_join_the_codec_catalog_under_the_local_trust_root() {
    const PACK_LIST: &[u8] = include_bytes!("../../msbe-wasm-codec/tests/fixtures/pack-list.wasm");
    let world = World::new();
    let key = world.inputs.join("publisher.toml").display().to_string();
    let module = world.file("pack-list.wasm", PACK_LIST);

    let generated = world.json(&["extension", "keygen", "publisher", key.as_str()]);
    assert_eq!(
        world
            .msbe(&["extension", "keygen", "publisher", key.as_str()])
            .code,
        exit::FAILURE,
        "a key file is never replaced"
    );
    let signed = world.json(&[
        "extension",
        "sign",
        module.as_str(),
        "--key",
        key.as_str(),
        "--version",
        "1.0.0",
    ]);
    assert_eq!(at(&signed, "/codec"), "pack-list");
    let envelope = at(&signed, "/envelope").as_str().unwrap().to_owned();

    let untrusted = world.msbe(&["extension", "verify", envelope.as_str()]);
    assert_eq!(untrusted.code, exit::FAILURE);
    assert!(untrusted.err.contains("not trusted"), "{}", untrusted.err);

    let trust = PathBuf::from(at(&generated, "/trust").as_str().unwrap());
    fs::create_dir_all(trust.parent().unwrap()).unwrap();
    fs::write(&trust, at(&generated, "/trust_entry").as_str().unwrap()).unwrap();
    let verified = world.json(&["extension", "verify", envelope.as_str()]);
    assert_eq!(at(&verified, "/signer"), "publisher");

    let codecs = world.home.join("extensions/codecs");
    fs::create_dir_all(&codecs).unwrap();
    fs::copy(&module, codecs.join("pack-list.wasm")).unwrap();
    fs::copy(&envelope, codecs.join("pack-list.toml")).unwrap();
    let formats = world.json(&["pack", "formats"]).to_string();
    assert!(formats.contains(r#""pack-list""#), "{formats}");

    fs::write(&trust, "").unwrap();
    let refused = world.msbe(&["pack", "formats"]);
    assert_eq!(refused.code, exit::FAILURE);
    assert!(refused.err.contains("pack-list.toml"), "{}", refused.err);
    let diagnosed = world.msbe(&["extension", "verify", envelope.as_str()]);
    assert!(
        diagnosed.err.contains("not trusted"),
        "extension commands still run to diagnose a refused codec: {}",
        diagnosed.err
    );
}

fn mrpack(world: &World, version: &str, files: &[(&str, &[u8])]) -> String {
    let mut index_files = Vec::new();
    for (file, bytes) in files {
        let url = format!("https://cdn.modrinth.test/{version}/{file}");
        world
            .modrinth
            .files
            .borrow_mut()
            .insert(url.clone(), bytes.to_vec());
        index_files.push(json!({
            "path": format!("mods/{file}"),
            "downloads": [url],
            "hashes": { "sha512": sha512_hex(bytes) },
            "env": { "client": "required", "server": "required" }
        }));
    }
    let index = json!({ "formatVersion": 1, "versionId": version, "files": index_files });
    world.zip(
        &format!("{version}.mrpack"),
        &[(
            "modrinth.index.json",
            serde_json::to_string(&index).unwrap().as_bytes(),
        )],
    )
}

fn items<'a>(preview: &'a Value, field: &str) -> Vec<&'a str> {
    at(preview, "/items")
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.get(field).and_then(Value::as_str).unwrap())
        .collect()
}

#[test]
fn native_bundles_are_deterministic_and_import_every_blob_to_the_same_deployment() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let local = world.file("sodium.jar", b"local sodium");
    world.json(&["add", "mc", local.as_str()]);
    world.json(&[
        "pack",
        "config",
        "set",
        "mc",
        "config/example.toml",
        "--content",
        "enabled = true\n",
    ]);

    let first = world.inputs.join("first.msbepack");
    let second = world.inputs.join("second.msbepack");
    for output in [&first, &second] {
        let report = world.json(&[
            "pack",
            "export",
            "mc",
            output.to_str().unwrap(),
            "--codec",
            "msbe-native",
            "--preset",
            "portable",
        ]);
        assert_eq!(at(&report, "/embedded"), 2);
    }
    assert_eq!(
        fs::read(&first).unwrap(),
        fs::read(&second).unwrap(),
        "repeated exports are byte-identical"
    );

    let game = world.game.display().to_string();
    let plan = world.plan.display().to_string();
    let store = world.inputs.join("copy-store").display().to_string();
    let added = world.msbe(&[
        "instance",
        "add",
        "copy",
        "--root",
        game.as_str(),
        "--plan",
        plan.as_str(),
        "--loader",
        "fabric",
        "--game-version",
        "1.21.1",
        "--store",
        store.as_str(),
    ]);
    assert_eq!(added.code, exit::OK, "{}", added.err);

    let bundle = first.to_str().unwrap();
    let preview = world.json(&[
        "pack",
        "import",
        "copy",
        bundle,
        "--profile",
        "fresh",
        "--dry-run",
    ]);
    assert_eq!(at(&preview, "/codec"), "msbe-native");
    assert_eq!(items(&preview, "action"), ["embedded", "embedded"]);
    let imported = world.json(&["pack", "import", "copy", bundle, "--profile", "fresh"]);
    assert_eq!(at(&imported, "/added"), &json!(["sodium"]));
    assert_eq!(
        deployment(&world, "copy", "fresh"),
        deployment(&world, "mc", "default"),
        "native import reaches the original deployment digest map"
    );
    let layers = at(
        &world.json(&["profile", "show", "copy", "fresh"]),
        "/layers",
    )
    .clone();
    assert_eq!(at(&layers, "/0/codec"), "msbe-native");
}

#[test]
fn export_policy_blocks_unknown_rights_publicly_and_unsourceable_content_when_thin() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let local = world.file("sodium.jar", b"local sodium");
    world.json(&["add", "mc", local.as_str()]);
    let output = world.inputs.join("public.msbepack").display().to_string();
    let export = |preset: &str, dry_run: bool| {
        let mut args = vec![
            "--format",
            "json",
            "pack",
            "export",
            "mc",
            output.as_str(),
            "--codec",
            "msbe-native",
            "--preset",
            preset,
        ];
        if dry_run {
            args.push("--dry-run");
        }
        world.msbe(&args)
    };

    let public = export("public-distribution", true);
    assert_eq!(public.code, exit::POLICY, "{}", public.err);
    let public: Value = serde_json::from_str(&public.out).unwrap();
    assert_eq!(at(&public, "/blockers/0/code"), "DistributionUnknown");
    assert_eq!(at(&public, "/blockers/0/path"), "mods/sodium.jar");

    let refused = export("public-distribution", false);
    assert_eq!(refused.code, exit::POLICY);
    assert!(
        refused.err.contains("DistributionUnknown"),
        "{}",
        refused.err
    );
    assert!(
        !Path::new(&output).exists(),
        "a blocked export writes nothing"
    );

    let thin = export("thin", true);
    assert_eq!(thin.code, exit::FAILURE, "{}", thin.err);
    let thin: Value = serde_json::from_str(&thin.out).unwrap();
    assert_eq!(at(&thin, "/blockers/0/code"), "UnreproducibleContent");

    let formats = world.json(&["pack", "formats", "--direction", "export"]);
    let ids: Vec<&str> = formats
        .as_array()
        .unwrap()
        .iter()
        .map(|codec| at(codec, "/id").as_str().unwrap())
        .collect();
    assert_eq!(ids, ["modrinth-mrpack", "msbe-native"]);
    let options = world.json(&["pack", "options", "msbe-native", "--preset", "complete"]);
    assert_eq!(at(&options, "/values/blob-mode"), "complete");
}

#[test]
fn snapshots_restore_every_blob_and_are_refused_as_packs() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let local = world.file("sodium.jar", b"local sodium");
    world.json(&["add", "mc", local.as_str()]);
    let snapshot = world.inputs.join("mc.msbesnapshot").display().to_string();
    let created = world.json(&["snapshot", "create", "mc", snapshot.as_str()]);
    assert_eq!(at(&created, "/blobs"), 1);

    let refused = world.msbe(&[
        "pack",
        "import",
        "mc",
        snapshot.as_str(),
        "--profile",
        "other",
    ]);
    assert_eq!(refused.code, exit::FAILURE);
    assert!(refused.err.contains("snapshot"), "{}", refused.err);
    let occupied = world.msbe(&["snapshot", "restore", snapshot.as_str(), "--dry-run"]);
    assert_eq!(
        occupied.code,
        exit::FAILURE,
        "an existing instance blocks a restore"
    );

    world.json(&["instance", "remove", "mc"]);
    world.json(&["snapshot", "restore", snapshot.as_str()]);
    let mods = at(&world.json(&["profile", "show", "mc"]), "/mods").clone();
    assert!(mods.get("sodium").is_some(), "{mods}");
    assert_eq!(world.msbe(&["deploy", "mc"]).code, exit::OK);
    assert_eq!(
        fs::read(world.game.join("mods/sodium.jar")).unwrap(),
        b"local sodium"
    );
}

#[test]
fn pack_updates_reapply_profile_changes_and_report_conflicts_for_resolution() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let v1 = mrpack(&world, "v1", &[("a.jar", b"a one"), ("b.jar", b"b one")]);
    world.json(&["pack", "import", "mc", v1.as_str()]);
    world.json(&["remove", "mc", "b"]);
    let local = world.file("c.jar", b"local c");
    world.json(&["add", "mc", local.as_str()]);

    let v2 = mrpack(&world, "v2", &[("a.jar", b"a two"), ("b.jar", b"b one")]);
    let preview = world.json(&["pack", "update", "mc", v2.as_str(), "--dry-run"]);
    assert_eq!(
        at(&preview, "/changes"),
        &json!([{"kind": "add-mod", "subject": "c"}, {"kind": "remove-mod", "subject": "b"}])
    );
    assert_eq!(at(&preview, "/conflicts"), &json!([]));
    assert_eq!(items(&preview, "action"), ["acquire", "reuse"]);
    let updated = world.json(&["pack", "update", "mc", v2.as_str()]);
    assert_eq!(at(&updated, "/mods"), &json!(["a", "c"]));
    world.json(&["deploy", "mc"]);
    assert_eq!(fs::read(world.game.join("mods/a.jar")).unwrap(), b"a two");
    assert!(!world.game.join("mods/b.jar").exists());

    let v3 = mrpack(&world, "v3", &[("a.jar", b"a two"), ("c.jar", b"pack c")]);
    let conflicted = world.msbe(&[
        "--format",
        "json",
        "pack",
        "update",
        "mc",
        v3.as_str(),
        "--dry-run",
    ]);
    assert_eq!(conflicted.code, exit::CONFLICT, "{}", conflicted.err);
    let conflicted: Value = serde_json::from_str(&conflicted.out).unwrap();
    let ids: Vec<&str> = at(&conflicted, "/conflicts")
        .as_array()
        .unwrap()
        .iter()
        .map(|conflict| at(conflict, "/id").as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mod:c", "mod:b"]);
    assert_eq!(
        world.msbe(&["pack", "update", "mc", v3.as_str()]).code,
        exit::CONFLICT,
        "unresolved conflicts are never silently kept or dropped"
    );

    let resolved = world.json(&[
        "pack",
        "update",
        "mc",
        v3.as_str(),
        "--resolve",
        "mod:c=keep",
        "--resolve",
        "mod:b=drop",
    ]);
    assert_eq!(at(&resolved, "/dropped"), &json!(["mod:b"]));
    world.json(&["deploy", "mc"]);
    assert_eq!(
        fs::read(world.game.join("mods/c.jar")).unwrap(),
        b"local c",
        "the kept change wins over the pack's file"
    );
}

#[test]
fn capture_adopts_changed_and_new_files_beneath_mutable_roots_only() {
    let world = World::new();
    world.add_instance_with_loader("forge", Some("1.7.10"));
    let archive = world.zip("tweaks.zip", &[("config/tweaks.cfg", b"speed=1\n")]);
    world.json(&["add", "mc", archive.as_str()]);
    world.json(&["deploy", "mc"]);
    fs::write(world.game.join("config/tweaks.cfg"), b"speed=2\n").unwrap();
    fs::write(world.game.join("config/extra.cfg"), b"extra=true\n").unwrap();
    fs::write(world.game.join("options.txt"), b"fov:90\n").unwrap();

    let preview = world.json(&["pack", "capture", "mc", "--dry-run"]);
    assert_eq!(
        items(&preview, "path"),
        ["config/extra.cfg", "config/tweaks.cfg"]
    );
    assert_eq!(items(&preview, "kind"), ["new", "changed"]);
    assert_eq!(
        at(&preview, "/items/1/diff"),
        &json!([{"kind": "removed", "text": "speed=1"}, {"kind": "added", "text": "speed=2"}])
    );
    let captured = world.json(&["pack", "capture", "mc"]);
    assert_eq!(
        at(&captured, "/captured"),
        &json!(["config/extra.cfg", "config/tweaks.cfg"])
    );
    assert_eq!(
        world
            .json(&["pack", "config", "list", "mc"])
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(
        at(
            &world.json(&["pack", "capture", "mc", "--dry-run"]),
            "/items"
        ),
        &json!([])
    );
}

#[test]
fn native_import_on_another_installation_fails_before_acquisition() {
    let world = World::new();
    world.add_instance(Some("1.21.1"));
    let local = world.file("sodium.jar", b"local sodium");
    world.json(&["add", "mc", local.as_str()]);
    let bundle = world.inputs.join("bundle.msbepack");
    world.json(&[
        "pack",
        "export",
        "mc",
        bundle.to_str().unwrap(),
        "--codec",
        "msbe-native",
    ]);
    let original = zip_entries(&bundle);
    let pinned = |digest: &str, name: &str| {
        let mut entries = original.clone();
        let lock = String::from_utf8(entries.remove("lock.toml").unwrap()).unwrap();
        let lock = format!(
            "{lock}\n[target.fingerprint]\nedition = \"retail\"\n\n[target.fingerprint.identifying]\n\"options.txt\" = \"sha256:{digest}\"\n"
        );
        entries.insert("lock.toml".to_owned(), lock.into_bytes());
        let borrowed: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(entry, bytes)| (entry.as_str(), bytes.as_slice()))
            .collect();
        world.zip(name, &borrowed)
    };

    let elsewhere = pinned(&"0".repeat(64), "elsewhere.msbepack");
    let refused = world.msbe(&[
        "--format",
        "json",
        "pack",
        "import",
        "mc",
        elsewhere.as_str(),
        "--profile",
        "fresh",
        "--dry-run",
    ]);
    assert_eq!(refused.code, exit::INTEGRITY, "{}", refused.err);
    let refused: Value = serde_json::from_str(&refused.out).unwrap();
    assert_eq!(at(&refused, "/blockers/0/code"), "EnvironmentMismatch");
    assert_eq!(at(&refused, "/blockers/0/path"), "options.txt");

    let here = pinned(&sha256_hex(b"fov:70\n"), "here.msbepack");
    let imported = world.json(&["pack", "import", "mc", here.as_str(), "--profile", "fresh"]);
    assert_eq!(at(&imported, "/added"), &json!(["sodium"]));
}
