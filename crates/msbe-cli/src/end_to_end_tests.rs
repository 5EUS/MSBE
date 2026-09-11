//! End to end: M1's local-file workflow against a synthetic Minecraft directory, driven
//! through the real command-line surface in-process, with the first-party plan.

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{exit, run};

struct World {
    _dir: TempDir,
    home: PathBuf,
    game: PathBuf,
    inputs: PathBuf,
    plan: PathBuf,
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
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(command_line, &mut out, &mut err);
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

    fn add_instance(&self) {
        let game = self.game.display().to_string();
        let plan = self.plan.display().to_string();
        let outcome = self.msbe(&[
            "instance",
            "add",
            "mc",
            "--root",
            game.as_str(),
            "--plan",
            plan.as_str(),
            "--loader",
            "fabric",
        ]);
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

#[test]
fn local_files_install_switch_repair_roll_back_and_purge_to_vanilla() {
    let world = World::new();
    let vanilla = snapshot(&world.game);
    world.add_instance();

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
    assert_eq!(
        world.json(&[
            "add",
            "mc",
            sodium.as_str(),
            lithium.as_str(),
            pack.as_str()
        ]),
        json!(["sodium-0.6.0", "lithium", "shader-pack"])
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

    // Something outside MSBE deletes a deployed file: verify reports it, deploy repairs it.
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
fn conflicting_mods_exit_with_the_conflict_code_and_change_nothing() {
    let world = World::new();
    let vanilla = snapshot(&world.game);
    world.add_instance();
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
}
