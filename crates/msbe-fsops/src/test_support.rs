//! Shared fixtures for this crate's tests.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::{Applier, Journal, RelPath, Store};

/// A small instance with vanilla files, including an executable, plus a store and journal
/// beside it on the same volume.
pub(crate) struct Fixture {
    _dir: TempDir,
    pub(crate) root: PathBuf,
    store: PathBuf,
    journal: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("instance");
        fs::create_dir_all(root.join("data/packs")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("data/packs/base.pak"), b"base pack").unwrap();
        fs::write(root.join("data/packs/extra.pak"), b"extra pack").unwrap();
        fs::write(root.join("bin/run.sh"), b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(root.join("bin/run.sh"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        Self {
            store: dir.path().join("store"),
            journal: dir.path().join("journal.jsonl"),
            root,
            _dir: dir,
        }
    }

    /// Opens an applier the way a freshly started process would.
    pub(crate) fn applier(&self) -> Applier {
        let store = Store::open(&self.store).unwrap();
        let journal = Journal::open(&self.journal).unwrap();
        Applier::open(&self.root, store, journal).unwrap()
    }
}

pub(crate) fn rel(path: &str) -> RelPath {
    RelPath::new(path).unwrap()
}

/// One entry in a directory snapshot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Dir,
    File { bytes: Vec<u8>, executable: bool },
}

/// Every entry under `root`, keyed by relative path. Leftover staging files appear as
/// extra entries, so an unclean recovery cannot compare equal.
pub(crate) fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn walk(root: &Path, dir: &Path, entries: &mut BTreeMap<String, Node>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let key = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            entries.insert(key, Node::Dir);
            walk(root, &path, entries);
        } else {
            entries.insert(
                key,
                Node::File {
                    bytes: fs::read(&path).unwrap(),
                    executable: executable(&meta),
                },
            );
        }
    }
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    (meta.permissions().mode() & 0o111) != 0
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> bool {
    false
}
