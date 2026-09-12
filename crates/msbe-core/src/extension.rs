//! Serving `run-extension` steps: pinning their modules beside a plan, and serving a mod and the
//! installation to the sandboxed step host.
//!
//! See `docs/18-wasm-extensions.md`.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use msbe_fsops::{Digest, RelPath, Store};
use msbe_plan_host::{Archive, Entry, Game, GameFile, ReadError};
use msbe_plan_schema::{ExtensionDeclaration, Loader, Plan};

use crate::instance::{InstanceError, StoredFile};

/// The most bytes an extension module may be.
pub(crate) const MODULE_LIMIT: u64 = 64 << 20;

/// Where the instance directory `instance` keeps the module `declaration` pins.
pub(crate) fn module_path(instance: &Path, declaration: &ExtensionDeclaration) -> PathBuf {
    instance
        .join("extensions")
        .join(format!("{}.wasm", declaration.sha256))
}

/// A declared extension and its verified module bytes.
pub(crate) type DeclaredModule<'a> = (&'a ExtensionDeclaration, Vec<u8>);

/// Reads every module `plan` declares from beside its manifest at `manifest`, each verified
/// against the SHA-256 the plan pins.
pub(crate) fn read_declared<'a>(
    plan: &'a Plan,
    manifest: &Path,
) -> Result<Vec<DeclaredModule<'a>>, InstanceError> {
    let directory = manifest.parent().unwrap_or_else(|| Path::new(""));
    plan.extensions
        .iter()
        .map(|declaration| {
            let bytes = read_module(&directory.join(&declaration.path), declaration)?;
            Ok((declaration, bytes))
        })
        .collect()
}

/// Reads the module at `path`, refusing one larger than [`MODULE_LIMIT`] or different from the
/// bytes `declaration` pins.
pub(crate) fn read_module(
    path: &Path,
    declaration: &ExtensionDeclaration,
) -> Result<Vec<u8>, InstanceError> {
    let io = |source| InstanceError::Io {
        op: "read extension module",
        path: path.to_path_buf(),
        source,
    };
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(io)?
        .take(MODULE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MODULE_LIMIT {
        return Err(InstanceError::ExtensionTooLarge {
            extension: declaration.id.clone(),
            limit: MODULE_LIMIT,
        });
    }
    let found = Digest::of_bytes(&bytes).to_string();
    if found.strip_prefix("sha256:") != Some(declaration.sha256.as_str()) {
        return Err(InstanceError::ExtensionMismatch {
            extension: declaration.id.clone(),
            expected: declaration.sha256.clone(),
            found,
        });
    }
    Ok(bytes)
}

/// The instance-relative directories `declaration` lets an extension write beneath for `loader`.
/// A target reference the loader does not declare contributes nothing.
pub(crate) fn roots(declaration: &ExtensionDeclaration, loader: &Loader) -> Vec<String> {
    if declaration.roots.is_empty() {
        return loader
            .targets
            .iter()
            .map(|target| target.path.clone())
            .collect();
    }
    declaration
        .roots
        .iter()
        .filter_map(|root| match root.strip_prefix("@loader.targets.") {
            Some(name) => loader
                .targets
                .iter()
                .find(|target| target.name == name)
                .map(|target| target.path.clone()),
            None => Some(root.clone()),
        })
        .collect()
}

/// One mod's files, served from the store.
pub(crate) struct ModArchive {
    store: Store,
    files: BTreeMap<String, (Digest, u64)>,
}

impl ModArchive {
    /// Serves `files` from the store shard at `store`.
    pub(crate) fn new(store: &Path, files: &[StoredFile]) -> Result<Self, InstanceError> {
        let store = Store::open(store)?;
        let files = files
            .iter()
            .map(|file| {
                let path = store.blob_path(&file.blob);
                let size = fs::metadata(&path)
                    .map_err(|source| InstanceError::Io {
                        op: "inspect",
                        path: path.clone(),
                        source,
                    })?
                    .len();
                Ok((file.source.as_str().to_owned(), (file.blob, size)))
            })
            .collect::<Result<_, InstanceError>>()?;
        Ok(Self { store, files })
    }
}

impl Archive for ModArchive {
    fn entries(&self) -> Vec<Entry> {
        self.files
            .iter()
            .map(|(path, (_, size))| Entry {
                path: path.clone(),
                size: *size,
            })
            .collect()
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, ReadError> {
        let (blob, _) = self.files.get(path).ok_or(ReadError::NotFound)?;
        read_blob(&self.store, blob)
    }
}

/// The game directory as it was before MSBE changed anything, read the way plan derivations read
/// it: a file a live deployment replaced is served as it was, and anything else from disk. Files
/// that are read are kept in the store, so their digests are the identities recorded as inputs.
pub(crate) struct Installation {
    root: PathBuf,
    store: Store,
    displaced: BTreeMap<RelPath, Option<Digest>>,
    seen: RefCell<BTreeMap<String, Option<Digest>>>,
}

impl Installation {
    /// The game at `root`, with its store shard at `store`. `displaced` holds what live
    /// deployments first displaced at each path: a file's digest, or `None` where nothing was.
    pub(crate) fn new(
        root: &Path,
        store: &Path,
        displaced: BTreeMap<RelPath, Option<Digest>>,
    ) -> Result<Self, InstanceError> {
        Ok(Self {
            root: root.to_path_buf(),
            store: Store::open(store)?,
            displaced,
            seen: RefCell::default(),
        })
    }

    fn digest(&self, path: &str) -> Result<Option<Digest>, ReadError> {
        if let Some(seen) = self.seen.borrow().get(path) {
            return Ok(*seen);
        }
        let relative = RelPath::new(path).map_err(failed)?;
        let digest = match self.displaced.get(&relative) {
            Some(displaced) => *displaced,
            None => match self.store.put_file(&relative.to_path(&self.root)) {
                Ok(digest) => Some(digest),
                Err(msbe_fsops::Error::Io { source, .. })
                    if matches!(
                        source.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::IsADirectory
                    ) =>
                {
                    None
                }
                Err(error) => return Err(failed(error)),
            },
        };
        self.seen.borrow_mut().insert(path.to_owned(), digest);
        Ok(digest)
    }
}

impl Game for Installation {
    fn stat(&self, path: &str) -> Result<Option<GameFile>, ReadError> {
        let Some(digest) = self.digest(path)? else {
            return Ok(None);
        };
        let size = fs::metadata(self.store.blob_path(&digest))
            .map_err(failed)?
            .len();
        Ok(Some(GameFile {
            size,
            identity: digest.to_string(),
        }))
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, ReadError> {
        let digest = self.digest(path)?.ok_or(ReadError::NotFound)?;
        read_blob(&self.store, &digest)
    }
}

fn read_blob(store: &Store, blob: &Digest) -> Result<Vec<u8>, ReadError> {
    let mut bytes = Vec::new();
    store
        .open_blob(blob)
        .map_err(failed)?
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    Ok(bytes)
}

fn failed(error: impl fmt::Display) -> ReadError {
    ReadError::Failed(error.to_string())
}
