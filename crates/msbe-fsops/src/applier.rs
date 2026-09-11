//! Executes operations against one instance root under the write-ahead journal.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use crate::{
    atomic,
    digest::Digest,
    error::{Error, IoResultExt, Result},
    journal::{Journal, Record, TxnId},
    materialize::{self, Backend},
    ops::{Operation, Prior},
    probe::Capabilities,
    relpath::RelPath,
    store::Store,
    sys,
};

/// A point within [`Applier::apply`], reported to an [`Observer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// The operation's record is durable in the journal; its path is untouched.
    Journaled {
        /// The operation's position in the transaction.
        index: usize,
    },
    /// A staged file sits beside the operation's path; the path itself is untouched.
    Staged {
        /// The operation's position in the transaction.
        index: usize,
    },
    /// The operation is visible at its path.
    Applied {
        /// The operation's position in the transaction.
        index: usize,
    },
    /// Every operation is applied; the commit record is not yet written.
    BeforeCommit,
}

/// Receives [`Checkpoint`]s during [`Applier::apply`], for progress reporting.
pub trait Observer {
    /// Called at each checkpoint.
    ///
    /// # Errors
    ///
    /// Any error stops the transaction at this point exactly as a crash would: nothing is
    /// cleaned up and nothing is committed. [`Applier::recover`] undoes it.
    fn checkpoint(&mut self, at: Checkpoint) -> Result<()>;
}

/// An [`Observer`] that ignores every checkpoint.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl Observer for NoopObserver {
    fn checkpoint(&mut self, _at: Checkpoint) -> Result<()> {
        Ok(())
    }
}

/// The result of a committed transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnReport {
    /// The committed transaction.
    pub txn: TxnId,
    /// The backend each materialized file actually used.
    pub backends: BTreeMap<RelPath, Backend>,
}

/// Applies operations to an instance root, backed by a store shard on the same volume.
#[derive(Debug)]
pub struct Applier {
    root: PathBuf,
    store: Store,
    journal: Journal,
    capabilities: Capabilities,
}

impl Applier {
    /// Opens an applier for the instance at `root` and probes what the shard can do there.
    ///
    /// If the journal may hold an interrupted transaction, call [`Applier::recover`] before
    /// applying anything; [`Applier::apply`] refuses until it has run.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `root` cannot be resolved or probed.
    pub fn open(root: &Path, store: Store, journal: Journal) -> Result<Self> {
        let root = fs::canonicalize(root).at("resolve", root)?;
        let capabilities = Capabilities::probe(&store, &root)?;
        Ok(Self {
            root,
            store,
            journal,
            capabilities,
        })
    }

    /// The canonical instance root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The store shard.
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// The journal.
    pub const fn journal(&self) -> &Journal {
        &self.journal
    }

    /// What the probe found when the applier was opened.
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Applies `operations` as one transaction and commits it.
    ///
    /// Missing parent directories are created by inserted [`Operation::CreateDir`] steps,
    /// so rollback removes them again.
    ///
    /// # Errors
    ///
    /// Invalid operations (a missing or corrupt blob, a directory in the way, a path that
    /// escapes the root) are rejected before anything is journaled. Any later failure leaves the
    /// transaction open, exactly as a crash would, for [`Applier::recover`] to undo.
    /// [`Error::RecoveryRequired`] means an earlier transaction is still open.
    pub fn apply(
        &mut self,
        operations: &[Operation],
        observer: &mut dyn Observer,
    ) -> Result<TxnReport> {
        if !self.journal.open_transactions().is_empty() {
            return Err(Error::RecoveryRequired);
        }
        let planned = self.plan(operations)?;
        let txn = self.journal.next_txn();
        self.journal.append(Record::Begin { txn })?;
        let mut backends = BTreeMap::new();
        for (index, operation) in planned.iter().enumerate() {
            if let Some(backend) = self.execute(txn, index, operation, observer)? {
                backends.insert(operation.path().clone(), backend);
            }
        }
        observer.checkpoint(Checkpoint::BeforeCommit)?;
        self.journal.append(Record::Commit { txn })?;
        Ok(TxnReport { txn, backends })
    }

    /// Undoes every transaction that began but never committed, newest first, and returns
    /// them.
    ///
    /// Idempotent: each undo step inspects the current state instead of assuming how far
    /// the interrupted operation got, so recovery can itself be interrupted and rerun.
    ///
    /// # Errors
    ///
    /// Returns the first error; the transaction stays open and recovery can be retried.
    pub fn recover(&mut self) -> Result<Vec<TxnId>> {
        let open = self.journal.open_transactions();
        for txn in open.iter().rev() {
            self.undo(*txn)?;
        }
        Ok(open)
    }

    /// Undoes a committed transaction. Only the most recent live transaction qualifies,
    /// since an older one's paths may since have changed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotLatest`], [`Error::NotCommitted`], [`Error::RecoveryRequired`],
    /// or any error from restoring a path.
    pub fn rollback(&mut self, txn: TxnId) -> Result<()> {
        if !self.journal.open_transactions().is_empty() {
            return Err(Error::RecoveryRequired);
        }
        let live = self.journal.live_transactions();
        match live.last() {
            Some(latest) if *latest == txn => self.undo(txn),
            _ if live.contains(&txn) => Err(Error::NotLatest { txn }),
            _ => Err(Error::NotCommitted { txn }),
        }
    }

    /// Validates operations and inserts directory creation for missing parents.
    fn plan(&self, operations: &[Operation]) -> Result<Vec<Operation>> {
        let mut planned = Vec::with_capacity(operations.len());
        let mut dirs = BTreeSet::new();
        for operation in operations {
            if let Operation::RemoveDir { path } = operation {
                self.plan_remove_dir(path, &mut planned)?;
                continue;
            }
            let path = operation.path();
            for ancestor in path.ancestors() {
                if dirs.contains(&ancestor) {
                    continue;
                }
                match stat(&self.resolve(&ancestor)?)? {
                    Kind::Dir => {}
                    Kind::Absent => planned.push(Operation::CreateDir {
                        path: ancestor.clone(),
                    }),
                    Kind::File { .. } => {
                        return Err(Error::NotADirectory {
                            path: ancestor.to_path(&self.root),
                        });
                    }
                }
                dirs.insert(ancestor);
            }
            let current = stat(&self.resolve(path)?)?;
            let occupied_by_dir = current == Kind::Dir || dirs.contains(path);
            match operation {
                Operation::CreateDir { .. } => {
                    if matches!(current, Kind::File { .. }) {
                        return Err(Error::NotADirectory {
                            path: path.to_path(&self.root),
                        });
                    }
                    if dirs.insert(path.clone()) {
                        planned.push(operation.clone());
                    }
                }
                Operation::Materialize { .. } | Operation::Remove { .. } if occupied_by_dir => {
                    return Err(Error::IsDirectory {
                        path: path.to_path(&self.root),
                    });
                }
                Operation::Materialize { .. } | Operation::Remove { .. } => {
                    planned.push(operation.clone());
                }
                // Planned above, before its ancestors could gain directory creation steps.
                Operation::RemoveDir { .. } => {}
            }
        }
        check_removed_dirs(&self.root, &planned)?;
        self.verify_blobs(&planned)?;
        Ok(planned)
    }

    /// Plans removing a directory that exists. An absent one needs nothing; a file there is
    /// an error.
    fn plan_remove_dir(&self, path: &RelPath, planned: &mut Vec<Operation>) -> Result<()> {
        match stat(&self.resolve(path)?)? {
            Kind::Dir => planned.push(Operation::RemoveDir { path: path.clone() }),
            Kind::Absent => {}
            Kind::File { .. } => {
                return Err(Error::NotADirectory {
                    path: path.to_path(&self.root),
                });
            }
        }
        Ok(())
    }

    /// Re-hashes every distinct blob about to be placed. A blob damaged by an in-place write
    /// through a hardlink must not spread into more files.
    fn verify_blobs(&self, operations: &[Operation]) -> Result<()> {
        let blobs: BTreeSet<Digest> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Materialize { blob, .. } => Some(*blob),
                Operation::CreateDir { .. }
                | Operation::Remove { .. }
                | Operation::RemoveDir { .. } => None,
            })
            .collect();
        blobs.iter().try_for_each(|blob| self.store.verify(blob))
    }

    /// Journals, then performs, one operation.
    fn execute(
        &mut self,
        txn: TxnId,
        index: usize,
        operation: &Operation,
        observer: &mut dyn Observer,
    ) -> Result<Option<Backend>> {
        let target = self.resolve(operation.path())?;
        let prior = self.capture(&target)?;
        self.journal.append(Record::Op {
            txn,
            index,
            operation: operation.clone(),
            prior: prior.clone(),
        })?;
        observer.checkpoint(Checkpoint::Journaled { index })?;
        let backend = match operation {
            Operation::CreateDir { .. } => {
                if prior == Prior::Absent {
                    fs::create_dir(&target).at("create directory", &target)?;
                    atomic::sync_parent(&target)?;
                }
                None
            }
            Operation::Materialize { blob, mutable, .. } => {
                let wanted = if *mutable {
                    Backend::Copy
                } else {
                    self.capabilities.choose(&Backend::DEFAULT_CHAIN)
                };
                let staged = sys::sibling(&target, &staging_tag(txn, index));
                let used = materialize::stage(&self.store.blob_path(blob), &staged, wanted)?;
                observer.checkpoint(Checkpoint::Staged { index })?;
                atomic::rename_displacing(&staged, &target)?;
                Some(used)
            }
            Operation::Remove { .. } => {
                if matches!(prior, Prior::File { .. }) {
                    sys::remove_file_if_exists(&target)?;
                    atomic::sync_parent(&target)?;
                }
                None
            }
            Operation::RemoveDir { .. } => {
                if prior == Prior::Dir && sys::remove_dir_if_empty(&target)? {
                    atomic::sync_parent(&target)?;
                }
                None
            }
        };
        observer.checkpoint(Checkpoint::Applied { index })?;
        Ok(backend)
    }

    /// Undoes a transaction's operations in reverse, then records the rollback.
    fn undo(&mut self, txn: TxnId) -> Result<()> {
        for (index, operation, prior) in self.journal.operations(txn).into_iter().rev() {
            let target = self.resolve(operation.path())?;
            if matches!(operation, Operation::Materialize { .. }) {
                sys::remove_file_if_exists(&sys::sibling(&target, &staging_tag(txn, index)))?;
            }
            self.restore(&target, &prior)?;
        }
        self.journal.append(Record::RolledBack { txn })
    }

    /// Returns `target` to `prior`, whatever state an interrupted operation left it in.
    fn restore(&self, target: &Path, prior: &Prior) -> Result<()> {
        match (prior, stat(target)?) {
            (Prior::Dir, Kind::Dir) | (Prior::Absent, Kind::Absent) => Ok(()),
            (Prior::Dir, Kind::Absent) => {
                fs::create_dir_all(target).at("create directory", target)?;
                atomic::sync_parent(target)
            }
            (Prior::Dir, Kind::File { .. }) => Err(Error::NotADirectory {
                path: target.to_path_buf(),
            }),
            (Prior::Absent, Kind::Dir) => {
                sys::remove_dir_if_empty(target)?;
                Ok(())
            }
            (Prior::Absent, Kind::File { .. }) => {
                sys::remove_file_if_exists(target)?;
                atomic::sync_parent(target)
            }
            (Prior::File { .. }, Kind::Dir) => Err(Error::IsDirectory {
                path: target.to_path_buf(),
            }),
            (
                Prior::File {
                    digest,
                    executable,
                    read_only,
                },
                current,
            ) => {
                // Untouched original: identical bytes and mode. A read-only file could be a
                // store blob linked into place, which would leave the instance sharing an
                // inode with the store, so one is always copied back.
                let untouched = !*read_only
                    && current
                        == Kind::File {
                            executable: *executable,
                            read_only: false,
                        };
                if untouched && hash_file(target)? == *digest {
                    return Ok(());
                }
                self.store.verify(digest)?;
                let staged = sys::sibling(target, "restore");
                materialize::stage(&self.store.blob_path(digest), &staged, Backend::Copy)?;
                if *executable {
                    make_executable(&staged)?;
                }
                if *read_only {
                    sys::make_read_only(&staged)?;
                }
                atomic::rename_displacing(&staged, target)
            }
        }
    }

    /// Captures what is at `target` so rollback can restore it.
    fn capture(&self, target: &Path) -> Result<Prior> {
        Ok(match stat(target)? {
            Kind::Absent => Prior::Absent,
            Kind::Dir => Prior::Dir,
            Kind::File {
                executable,
                read_only,
            } => Prior::File {
                digest: self.store.put_file(target)?,
                executable,
                read_only,
            },
        })
    }

    /// Joins `path` onto the root, confirming every existing ancestor resolves inside the
    /// root so a symlinked directory cannot redirect a write elsewhere.
    fn resolve(&self, path: &RelPath) -> Result<PathBuf> {
        for ancestor in path.ancestors() {
            let dir = ancestor.to_path(&self.root);
            match fs::canonicalize(&dir) {
                Ok(real) if real.starts_with(&self.root) => {}
                Ok(_) => return Err(Error::EscapesRoot { path: dir }),
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(source) => {
                    return Err(Error::Io {
                        op: "resolve",
                        path: dir,
                        source,
                    });
                }
            }
        }
        Ok(path.to_path(&self.root))
    }
}

/// What a path currently is. Symlinks and special files are refused, never followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Absent,
    File { executable: bool, read_only: bool },
    Dir,
}

fn stat(target: &Path) -> Result<Kind> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => Ok(Kind::Dir),
        Ok(meta) if meta.is_file() => Ok(Kind::File {
            executable: is_executable(&meta),
            read_only: meta.permissions().readonly(),
        }),
        Ok(_) => Err(Error::EscapesRoot {
            path: target.to_path_buf(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Kind::Absent),
        Err(source) => Err(Error::Io {
            op: "inspect",
            path: target.to_path_buf(),
            source,
        }),
    }
}

/// Refuses a transaction that removes a directory while also creating or placing something
/// inside it. Removing files inside it is allowed: that is how a directory gets emptied.
fn check_removed_dirs(root: &Path, planned: &[Operation]) -> Result<()> {
    let removed: BTreeSet<&RelPath> = planned
        .iter()
        .filter_map(|operation| match operation {
            Operation::RemoveDir { path } => Some(path),
            Operation::CreateDir { .. }
            | Operation::Materialize { .. }
            | Operation::Remove { .. } => None,
        })
        .collect();
    let conflict = planned
        .iter()
        .filter(|operation| {
            matches!(
                operation,
                Operation::CreateDir { .. } | Operation::Materialize { .. }
            )
        })
        .map(Operation::path)
        .find(|path| {
            removed.contains(*path)
                || path
                    .ancestors()
                    .iter()
                    .any(|ancestor| removed.contains(ancestor))
        });
    match conflict {
        Some(path) => Err(Error::DirectoryInUse {
            path: path.to_path(root),
        }),
        None => Ok(()),
    }
}

fn staging_tag(txn: TxnId, index: usize) -> String {
    format!("{}-{index}", txn.get())
}

fn hash_file(path: &Path) -> Result<Digest> {
    let file = File::open(path).at("open", path)?;
    Digest::of_reader(BufReader::new(file)).at("read", path)
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    (meta.permissions().mode() & 0o111) != 0
}

#[cfg(not(unix))]
const fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = fs::metadata(path).at("stat", path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions).at("set permissions", path)
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "mirrors the Unix signature, which can fail, so the call site stays platform-agnostic"
)]
const fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Checkpoint;
    use crate::{
        Backend, Digest, Error, NoopObserver, Observer, Operation, Result,
        test_support::{Fixture, rel, snapshot},
    };

    /// Aborts at the first checkpoint.
    struct Abort;

    impl Observer for Abort {
        fn checkpoint(&mut self, at: Checkpoint) -> Result<()> {
            Err(Error::Aborted(at))
        }
    }

    /// An operation paired with a predicate its rejection must satisfy.
    type Case = (Operation, fn(&Error) -> bool);

    fn place(path: &str, blob: Digest, mutable: bool) -> Operation {
        Operation::Materialize {
            path: rel(path),
            blob,
            mutable,
        }
    }

    #[test]
    fn apply_creates_missing_parents_and_commits() {
        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"module").unwrap();
        let report = applier
            .apply(
                &[place("mods/a/b/module.pak", blob, false)],
                &mut NoopObserver,
            )
            .unwrap();

        assert_eq!(
            fs::read(fixture.root.join("mods/a/b/module.pak")).unwrap(),
            b"module"
        );
        assert!(applier.journal().open_transactions().is_empty());
        assert_eq!(applier.journal().live_transactions(), [report.txn]);
        let expected = applier.capabilities().choose(&Backend::DEFAULT_CHAIN);
        assert_eq!(
            report.backends.get(&rel("mods/a/b/module.pak")),
            Some(&expected)
        );
    }

    #[test]
    fn mutable_files_are_copied_and_writes_never_reach_the_store() {
        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"enabled = true\n").unwrap();
        let report = applier
            .apply(&[place("settings.cfg", blob, true)], &mut NoopObserver)
            .unwrap();
        assert_eq!(
            report.backends.get(&rel("settings.cfg")),
            Some(&Backend::Copy)
        );
        fs::write(fixture.root.join("settings.cfg"), b"enabled = false\n").unwrap();
        applier.store().verify(&blob).unwrap();
    }

    #[test]
    fn rollback_restores_the_exact_prior_state() {
        let fixture = Fixture::new();
        let original = snapshot(&fixture.root);
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"replacement").unwrap();
        let report = applier
            .apply(
                &[
                    place("data/packs/base.pak", blob, false),
                    place("bin/run.sh", blob, false),
                    Operation::Remove {
                        path: rel("data/packs/extra.pak"),
                    },
                    place("mods/new/extra.pak", blob, false),
                ],
                &mut NoopObserver,
            )
            .unwrap();
        assert_ne!(snapshot(&fixture.root), original);

        applier.rollback(report.txn).unwrap();
        assert_eq!(snapshot(&fixture.root), original);
    }

    /// On Windows a hardlink shares the read-only attribute of its store blob, and replacing a
    /// read-only file is refused, so this is the path that breaks there if anything does.
    #[test]
    fn read_only_files_and_linked_blobs_are_replaced_and_restored_exactly() {
        let fixture = Fixture::new();
        let original = snapshot(&fixture.root);
        let mut applier = fixture.applier();
        let first = applier.store().put_bytes(b"first").unwrap();
        let second = applier.store().put_bytes(b"second").unwrap();
        let deploy = |blob| {
            [
                place("data/packs/locked.pak", blob, false),
                place("mods/linked.pak", blob, false),
            ]
        };

        let one = applier.apply(&deploy(first), &mut NoopObserver).unwrap();
        let two = applier.apply(&deploy(second), &mut NoopObserver).unwrap();
        assert_eq!(
            fs::read(fixture.root.join("mods/linked.pak")).unwrap(),
            b"second"
        );

        applier.rollback(two.txn).unwrap();
        assert_eq!(
            fs::read(fixture.root.join("data/packs/locked.pak")).unwrap(),
            b"first"
        );
        applier.rollback(one.txn).unwrap();
        assert_eq!(snapshot(&fixture.root), original);
        for blob in [first, second] {
            applier.store().verify(&blob).unwrap();
            let meta = fs::metadata(applier.store().blob_path(&blob)).unwrap();
            assert!(
                meta.permissions().readonly(),
                "a store blob became writable"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_corrupt_blob_is_refused_until_its_content_is_added_again() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"pristine").unwrap();
        let path = applier.store().blob_path(&blob);
        // What a game writing in place through a hardlink does to the shared blob.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, b"tampered").unwrap();

        let operations = [place("mods/module.pak", blob, false)];
        let error = applier.apply(&operations, &mut NoopObserver).unwrap_err();
        assert!(
            matches!(error, Error::Corrupt { expected, .. } if expected == blob),
            "{error:?}"
        );
        assert!(applier.journal().records().is_empty());
        assert!(!fixture.root.join("mods").exists());

        applier.store().put_bytes(b"pristine").unwrap();
        applier.apply(&operations, &mut NoopObserver).unwrap();
        assert_eq!(
            fs::read(fixture.root.join("mods/module.pak")).unwrap(),
            b"pristine"
        );
    }

    #[test]
    fn empty_directories_are_removed_and_restored_but_never_ones_with_contents() {
        let fixture = Fixture::new();
        let original = snapshot(&fixture.root);
        let mut applier = fixture.applier();
        let report = applier
            .apply(
                &[
                    Operation::RemoveDir { path: rel("logs") },
                    Operation::RemoveDir {
                        path: rel("data/packs"),
                    },
                    Operation::RemoveDir {
                        path: rel("never-existed"),
                    },
                ],
                &mut NoopObserver,
            )
            .unwrap();
        assert!(!fixture.root.join("logs").exists());
        assert!(fixture.root.join("data/packs/base.pak").is_file());

        applier.rollback(report.txn).unwrap();
        assert_eq!(snapshot(&fixture.root), original);
    }

    #[test]
    fn a_transaction_cannot_write_inside_a_directory_it_removes() {
        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"x").unwrap();
        let error = applier
            .apply(
                &[
                    Operation::RemoveDir { path: rel("logs") },
                    place("logs/new.log", blob, false),
                ],
                &mut NoopObserver,
            )
            .unwrap_err();
        assert!(matches!(error, Error::DirectoryInUse { .. }), "{error:?}");
        assert!(applier.journal().records().is_empty());
    }

    #[test]
    fn only_the_latest_live_transaction_can_be_rolled_back() {
        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"x").unwrap();
        let first = applier
            .apply(&[place("one.pak", blob, false)], &mut NoopObserver)
            .unwrap();
        let second = applier
            .apply(&[place("two.pak", blob, false)], &mut NoopObserver)
            .unwrap();

        assert!(matches!(
            applier.rollback(first.txn),
            Err(Error::NotLatest { .. })
        ));
        applier.rollback(second.txn).unwrap();
        assert!(matches!(
            applier.rollback(second.txn),
            Err(Error::NotCommitted { .. })
        ));
        applier.rollback(first.txn).unwrap();
    }

    #[test]
    fn invalid_operations_are_rejected_before_anything_is_journaled() {
        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let missing = Digest::of_bytes(b"never stored");
        let blob = applier.store().put_bytes(b"x").unwrap();

        let cases: [Case; 3] = [
            (place("x.pak", missing, false), |e| {
                matches!(e, Error::MissingBlob(_))
            }),
            (place("data/packs", blob, false), |e| {
                matches!(e, Error::IsDirectory { .. })
            }),
            (place("data/packs/base.pak/inner", blob, false), |e| {
                matches!(e, Error::NotADirectory { .. })
            }),
        ];
        for (operation, expected) in cases {
            let error = applier
                .apply(std::slice::from_ref(&operation), &mut NoopObserver)
                .unwrap_err();
            assert!(expected(&error), "{operation:?} gave {error:?}");
        }
        assert!(applier.journal().records().is_empty());
    }

    #[test]
    fn an_open_transaction_blocks_new_work_until_recovered() {
        let fixture = Fixture::new();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"x").unwrap();
        let operations = [place("x.pak", blob, false)];

        applier.apply(&operations, &mut Abort).unwrap_err();
        assert!(matches!(
            applier.apply(&operations, &mut NoopObserver),
            Err(Error::RecoveryRequired)
        ));
        assert_eq!(applier.recover().unwrap().len(), 1);
        applier.apply(&operations, &mut NoopObserver).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_symlink_out_of_the_root() {
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.root.join("mods")).unwrap();
        let mut applier = fixture.applier();
        let blob = applier.store().put_bytes(b"x").unwrap();

        let error = applier
            .apply(&[place("mods/escape.pak", blob, false)], &mut NoopObserver)
            .unwrap_err();
        assert!(matches!(error, Error::EscapesRoot { .. }), "{error:?}");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
